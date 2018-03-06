use webgl::*;
use webgl;

use na::*;
use std::rc::{Rc, Weak};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;
use std::ops::{Deref, DerefMut};

use engine::core::{Component, ComponentBased, GameObject, SceneTree};
use engine::render::Camera;
use engine::render::{Directional, Light};
use engine::render::{Material, Mesh, MeshBuffer, MeshSurface, ShaderProgram, Texture};
use engine::render::RenderQueue;
use engine::asset::{AssetError, AssetResult, AssetSystem};

use std::default::Default;

use super::imgui;

pub trait IEngine {
    fn new_game_object(&mut self, parent: &GameObject) -> Rc<RefCell<GameObject>>;

    fn asset_system<'a>(&'a self) -> &'a AssetSystem;

    fn asset_system_mut<'a>(&'a mut self) -> &'a mut AssetSystem;

    fn gui_context(&mut self) -> Rc<RefCell<imgui::Context>>;

    fn screen_size(&self) -> (u32, u32);

    fn hidpi_factor(&self) -> f32;
}

pub struct Engine<A>
where
    A: AssetSystem,
{
    pub gl: WebGLRenderingContext,
    pub main_camera: Option<Rc<RefCell<Camera>>>,

    pub objects: Vec<Weak<RefCell<GameObject>>>,
    pub program_cache: RefCell<HashMap<&'static str, Rc<ShaderProgram>>>,
    pub asset_system: Box<A>,
    pub screen_size: (u32, u32),
    pub hidpi: f32,

    pub gui_context: Rc<RefCell<imgui::Context>>,
}

#[derive(Default)]
struct EngineContext {
    mesh_buffer: Weak<MeshBuffer>,
    prog: Weak<ShaderProgram>,
    textures: VecDeque<(u32, Weak<Texture>)>,

    main_light: Option<Arc<Component>>,
    point_lights: Vec<Arc<Component>>,

    switch_mesh: u32,
    switch_prog: u32,
    switch_tex: u32,
}

macro_rules! impl_cacher {
    ($k:ident, $t:ty) => {
        impl EngineCacher for $t {
            fn get_cache<'a>(ctx: &'a mut EngineContext) -> &'a mut Weak<Self> {
                &mut ctx.$k
            }
        }
    };
}

trait EngineCacher {
    fn get_cache(ctx: &mut EngineContext) -> &mut Weak<Self>;
}

impl_cacher!(prog, ShaderProgram);
impl_cacher!(mesh_buffer, MeshBuffer);

const MAX_TEXTURE_UNITS: u32 = 8;

impl EngineContext {
    pub fn prepare_cache<T, F>(&mut self, new_p: &Rc<T>, bind: F) -> AssetResult<()>
    where
        T: EngineCacher,
        F: FnOnce(&mut EngineContext) -> AssetResult<()>,
    {
        if self.need_cache(new_p) {
            bind(self)?;
            *T::get_cache(self) = Rc::downgrade(new_p);
        }

        Ok(())
    }

    pub fn need_cache_tex(&self, new_tex: &Rc<Texture>) -> Option<u32> {
        for &(u, ref tex) in self.textures.iter() {
            if let Some(ref p) = tex.upgrade() {
                if Rc::ptr_eq(new_tex, p) {
                    return Some(u);
                }
            }
        }

        None
    }

    pub fn prepare_cache_tex<F>(&mut self, new_tex: &Rc<Texture>, bind_func: F) -> AssetResult<u32>
    where
        F: FnOnce(&mut EngineContext, u32) -> AssetResult<()>,
    {
        let found = self.need_cache_tex(new_tex);
        if let Some(t) = found {
            return Ok(t);
        }

        let mut unit = self.textures.len() as u32;

        // find the empty slots.
        if unit >= MAX_TEXTURE_UNITS {
            let opt_pos = self.textures
                .iter()
                .position(|&(_, ref t)| t.upgrade().is_none());

            unit = match opt_pos {
                Some(pos) => self.textures.remove(pos).unwrap().0,
                None => self.textures.pop_front().unwrap().0,
            }
        }

        bind_func(self, unit).map(|_| {
            self.textures.push_back((unit, Rc::downgrade(new_tex)));
            unit
        })
    }

    fn need_cache<T>(&mut self, new_p: &Rc<T>) -> bool
    where
        T: EngineCacher,
    {
        match T::get_cache(self).upgrade() {
            None => true,
            Some(ref p) => !Rc::ptr_eq(new_p, p),
        }
    }
}

struct RenderCommand {
    pub surface: Rc<MeshSurface>,
    pub model_m: Matrix4<f32>,
    pub cam_distance: f32,
}

#[allow(dead_code)]
enum DepthTest {
    Never,
    Less,
    Equal,
    LessEqual,
    Greater,
    NotEqual,
    GreaterEqual,
    Always,
}

impl Default for DepthTest {
    fn default() -> DepthTest {
        DepthTest::Less
    }
}

impl DepthTest {
    fn as_gl_state(&self) -> webgl::DepthTest {
        match self {
            &DepthTest::Never => webgl::DepthTest::Never,
            &DepthTest::Always => webgl::DepthTest::Always,
            &DepthTest::Less => webgl::DepthTest::Less,
            &DepthTest::LessEqual => webgl::DepthTest::Lequal,
            &DepthTest::Greater => webgl::DepthTest::Greater,
            &DepthTest::NotEqual => webgl::DepthTest::Notequal,
            &DepthTest::GreaterEqual => webgl::DepthTest::Gequal,
            &DepthTest::Equal => webgl::DepthTest::Equal,
        }
    }
}

#[derive(Default)]
struct RenderQueueState {
    depth_write: bool,
    depth_test: bool,
    depth_func: DepthTest,
    commands: Vec<RenderCommand>,
}

impl RenderQueueState {
    fn sort_by_cam_distance(&mut self) {
        self.commands.sort_unstable_by(|a, b| {
            let adist: f32 = a.cam_distance;
            let bdist: f32 = b.cam_distance;

            bdist.partial_cmp(&adist).unwrap()
        });
    }
}

#[derive(Default)]
struct RenderQueueList(BTreeMap<RenderQueue, RenderQueueState>);

impl RenderQueueList {
    pub fn new() -> RenderQueueList {
        let mut qlist = RenderQueueList::default();

        let mut state = RenderQueueState::default();
        state.depth_write = true;
        state.depth_test = true;
        qlist.insert(RenderQueue::Opaque, state);

        let mut state = RenderQueueState::default();
        state.depth_write = false;
        state.depth_test = true;
        state.depth_func = DepthTest::LessEqual;
        qlist.insert(RenderQueue::Skybox, state);

        let mut state = RenderQueueState::default();
        state.depth_write = false;
        state.depth_test = true;
        qlist.insert(RenderQueue::Transparent, state);

        qlist
    }
}

impl Deref for RenderQueueList {
    type Target = BTreeMap<RenderQueue, RenderQueueState>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for RenderQueueList {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

fn compute_model_m(object: &GameObject) -> Matrix4<f32> {
    object.transform.as_global_matrix()
}

pub struct ClearOption {
    pub color: Option<(f32, f32, f32, f32)>,
    pub clear_color: bool,
    pub clear_depth: bool,
    pub clear_stencil: bool,
}

impl Default for ClearOption {
    fn default() -> Self {
        ClearOption {
            color: Some((0.3, 0.3, 0.3, 1.0)),
            clear_color: true,
            clear_depth: true,
            clear_stencil: false,
        }
    }
}

impl<A> Engine<A>
where
    A: AssetSystem,
{
    pub fn new_scene_tree(&self) -> Rc<SceneTree> {
        SceneTree::new()
    }

    pub fn clear(&self, option: ClearOption) {
        // make sure all reset all state
        self.gl.depth_mask(true);

        if let Some(col) = option.color {
            self.gl.clear_color(col.0, col.1, col.2, col.3);
        } else {
            self.gl.clear_color(0.0, 0.0, 0.0, 1.0);
        }

        if option.clear_color {
            self.gl.clear(BufferBit::Color);
        }
        if option.clear_depth {
            self.gl.clear(BufferBit::Depth);
        }
        if option.clear_stencil {
            self.gl.clear(BufferBit::Stencil);
        }
    }

    pub fn resize(&mut self, size: (u32, u32)) {
        self.screen_size = size;

        self.gui_context.borrow_mut().reset();
    }

    fn setup_material(&self, ctx: &mut EngineContext, material: &Material) -> AssetResult<()> {
        ctx.prepare_cache(&material.program, |ctx| {
            material.program.bind(&self.gl)?;
            ctx.switch_prog += 1;
            Ok(())
        })?;

        let prog = ctx.prog.upgrade().unwrap();

        material.bind(|tex| {
            ctx.prepare_cache_tex(tex, |ctx, unit| {
                // Binding texture
                tex.bind(&self.gl, unit)?;

                ctx.switch_tex += 1;
                Ok(())
            })
        })?;

        self.setup_light(ctx, &prog);

        Ok(())
    }

    fn setup_camera(&self, ctx: &mut EngineContext, modelm: Matrix4<f32>, camera: &Camera) {
        let prog = ctx.prog.upgrade().unwrap();
        // setup_camera
        let perspective = camera.perspective(self.screen_size);

        prog.set("uMVMatrix", camera.v * modelm);
        prog.set("uPMatrix", perspective);

        let skybox_v = camera.v.fixed_slice::<U3, U3>(0, 0);
        let mut skybox_v = skybox_v.fixed_resize::<U4, U4>(0.0);
        skybox_v.data[15] = 1.0;

        prog.set("uPVMatrix", perspective * camera.v);
        prog.set("uPVSkyboxMatrix", perspective * skybox_v);

        prog.set("uNMatrix", modelm.try_inverse().unwrap().transpose());
        prog.set("uMMatrix", modelm);
        prog.set("uViewPos", camera.eye());
    }

    fn setup_light(&self, ctx: &EngineContext, prog: &ShaderProgram) {
        // Setup light

        let light_com = ctx.main_light.as_ref().unwrap();
        let light = light_com.try_as::<Light>().unwrap();
        light.borrow().bind("uDirectionalLight", &prog);

        for (i, plight_com) in ctx.point_lights.iter().enumerate() {
            let plight = plight_com.try_as::<Light>().unwrap();
            let name = format!("uPointLights[{}]", i);

            plight.borrow().bind(&name, &prog);
        }
    }

    fn render_commands(&self, ctx: &mut EngineContext, q: &RenderQueueState, camera: &Camera) {
        let gl = &self.gl;

        if q.depth_test {
            gl.enable(Flag::DepthTest as i32);
            gl.depth_func(q.depth_func.as_gl_state());
        } else {
            gl.disable(Flag::DepthTest as i32);
        }

        gl.depth_mask(q.depth_write);

        for cmd in q.commands.iter() {
            if let Err(err) = self.setup_material(ctx, &*cmd.surface.material) {
                if let AssetError::NotReady = err {
                    continue;
                }

                panic!(format!("Failed to load material, reason {:?}", err));
            }

            let prog = ctx.prog.upgrade().unwrap();

            let r = ctx.prepare_cache(&cmd.surface.buffer, |ctx| {
                cmd.surface.buffer.bind(&self.gl, &prog)?;
                ctx.switch_mesh += 1;
                Ok(())
            });

            match r {
                Ok(_) => {
                    self.setup_camera(ctx, cmd.model_m, camera);
                    prog.commit(gl);
                    cmd.surface.buffer.render(gl);
                    cmd.surface.buffer.unbind(gl);
                }
                Err(ref err) => match *err {
                    AssetError::NotReady => (),
                    _ => panic!(format!("Failed to load mesh, reason {:?}", err)),
                },
            }
        }
    }

    fn map_component<T, F>(&self, mut func: F)
    where
        T: 'static + ComponentBased,
        F: FnMut(Arc<Component>) -> bool,
    {
        for obj in self.objects.iter() {
            let result = obj.upgrade().and_then(|obj| {
                obj.try_borrow()
                    .ok()
                    .and_then(|o| o.find_component::<T>().map(|(_, c)| c))
            });

            if let Some(com) = result {
                if !func(com) {
                    return;
                }
            }
        }
    }

    fn find_all_components<T>(&self) -> Vec<Arc<Component>>
    where
        T: 'static + ComponentBased,
    {
        let mut result = Vec::new();
        self.map_component::<T, _>(|c| {
            result.push(c);
            true
        });

        result
    }

    fn find_component<T>(&self) -> Option<Arc<Component>>
    where
        T: 'static + ComponentBased,
    {
        let mut r = None;
        self.map_component::<T, _>(|c| {
            r = Some(c);
            false
        });

        r
    }

    fn prepare_ctx(&self, ctx: &mut EngineContext) {
        // prepare main light.
        ctx.main_light = Some(
            self.find_component::<Light>()
                .unwrap_or({ Component::new(Light::new(Directional::default())) }),
        );

        ctx.point_lights = self.find_all_components::<Light>()
                .into_iter()
                .filter(|c| {
                    let light_com = c.try_as::<Light>().unwrap();
                    match *light_com.borrow() {
                        Light::Point(_) => true,
                        _ => false,
                    }
                })
                .take(4)            // only take 4 points light.
                .collect();
    }

    fn gather_render_commands(
        &self,
        object: &GameObject,
        cam_pos: &Vector3<f32>,
        render_q: &mut RenderQueueList,
    ) {
        if !object.active {
            return;
        }

        let result = object.find_component::<Mesh>();

        if let Some((mesh, _)) = result {
            for surface in mesh.surfaces.iter() {
                let q = render_q.get_mut(&surface.material.render_queue).unwrap();

                let cam_dist =
                    (cam_pos - object.transform.global().translation.vector).norm_squared();

                q.commands.push(RenderCommand {
                    surface: surface.clone(),
                    model_m: compute_model_m(&*object),
                    cam_distance: cam_dist,
                })
            }
        }
    }

    pub fn render_pass(&self, camera: &Camera, clear_option: ClearOption) {
        let objects = &self.objects;

        let mut ctx: EngineContext = Default::default();

        if let Some(ref rt) = camera.render_texture {
            rt.bind_frame_buffer(&self.gl);
        }

        match camera.rect {
            Some(((x, y), (w, h))) => {
                self.gl.viewport(x, y, w, h);
            }
            None => {
                self.gl
                    .viewport(0, 0, self.screen_size.0, self.screen_size.1);
            }
        }

        self.clear(clear_option);

        self.prepare_ctx(&mut ctx);

        let mut render_q = RenderQueueList::new();

        // gather commands
        for obj in objects.iter() {
            obj.upgrade().map(|obj| {
                if let Ok(object) = obj.try_borrow() {
                    self.gather_render_commands(&object, &camera.eye(), &mut render_q)
                }
            });
        }

        // Sort the transparent queue
        render_q
            .get_mut(&RenderQueue::Transparent)
            .unwrap()
            .sort_by_cam_distance();

        for (_, q) in render_q.iter() {
            self.render_commands(&mut ctx, &q, camera);
        }

        if let Some(ref rt) = camera.render_texture {
            rt.unbind_frame_buffer(&self.gl);
        }
    }

    pub fn render(&mut self, clear_option: ClearOption) {
        imgui::pre_render(self);

        if let Some(ref camera) = self.main_camera.as_ref() {
            self.render_pass(&camera.borrow(), clear_option);
        } else {
            // We dont have a main camera here, just clean the screen.
            self.clear(clear_option);
        }
    }

    pub fn new(webgl_ctx: WebGLContext, size: (u32, u32), hidpi: f32) -> Engine<A> {
        let gl = WebGLRenderingContext::new(webgl_ctx);

        /*=========Drawing the triangle===========*/

        // Clear the canvas
        gl.clear_color(0.5, 0.5, 0.5, 1.0);

        // Enable the depth test
        gl.enable(Flag::DepthTest as i32);

        // Enable alpha blending
        gl.enable(Flag::Blend as i32);

        gl.enable(Culling::CullFace as i32);
        gl.cull_face(Culling::Back);

        // Clear the color buffer bit
        gl.clear(BufferBit::Color);
        gl.clear(BufferBit::Depth);
        gl.blend_func(BlendMode::SrcAlpha, BlendMode::OneMinusSrcAlpha);

        // Set the view port
        gl.viewport(0, 0, size.0, size.1);

        let gui_tree = SceneTree::new();

        Engine {
            gl: gl,
            main_camera: None,
            objects: vec![],
            program_cache: RefCell::new(HashMap::new()),
            asset_system: Box::new(A::new()),
            gui_context: Rc::new(RefCell::new(imgui::Context::new(gui_tree))),
            screen_size: size,
            hidpi: hidpi,
        }
    }

    pub fn begin(&mut self) {
        imgui::begin();

        self.asset_system_mut().step();
    }

    pub fn end(&mut self) {
        // drop all gameobjects if there are no other references
        self.objects.retain(|obj| obj.upgrade().is_some());
    }
}

impl<A: AssetSystem> IEngine for Engine<A> {
    fn new_game_object(&mut self, parent: &GameObject) -> Rc<RefCell<GameObject>> {
        let go = parent.tree().new_node(parent);

        self.objects.push(Rc::downgrade(&go));
        go
    }

    fn gui_context(&mut self) -> Rc<RefCell<imgui::Context>> {
        self.gui_context.clone()
    }

    fn asset_system<'a>(&'a self) -> &'a AssetSystem {
        &*self.asset_system
    }

    fn asset_system_mut<'a>(&'a mut self) -> &'a mut AssetSystem {
        &mut *self.asset_system
    }

    fn screen_size(&self) -> (u32, u32) {
        self.screen_size
    }

    fn hidpi_factor(&self) -> f32 {
        self.hidpi
    }
}
