use webgl::*;
use uni_app::App;

use na::*;
use std::rc::{Rc, Weak};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use super::core::{Component, ComponentBased};
use super::{Camera, DirectionalLight, GameObject, Light, Material, Mesh, MeshBuffer,
            ShaderProgram, Texture};
use super::asset::{AssetDatabase, AssetSystem};

use super::imgui;

pub trait IEngine {
    fn new_gameobject(&mut self) -> Rc<RefCell<GameObject>>;

    fn asset_system<'a>(&'a self) -> &'a AssetSystem;

    fn gui_context(&mut self) -> Rc<RefCell<imgui::Context>>;
}

pub struct Engine<A = AssetDatabase>
where
    A: AssetSystem,
{
    pub gl: WebGLRenderingContext,
    pub main_camera: Option<Camera>,

    pub objects: Vec<Weak<RefCell<GameObject>>>,
    pub program_cache: RefCell<HashMap<&'static str, Rc<ShaderProgram>>>,
    pub asset_system: Rc<A>,

    pub gui_context: Rc<RefCell<imgui::Context>>,
}

#[derive(Default)]
struct EngineContext {
    mesh_buffer: Weak<MeshBuffer>,
    prog: Weak<ShaderProgram>,
    tex: Weak<Texture>,

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
impl_cacher!(tex, Texture);

impl EngineContext {
    pub fn prepare_cache<T, F>(&mut self, new_p: &Rc<T>, bind: F) -> Result<(), &'static str>
    where
        T: EngineCacher,
        F: FnOnce(&mut EngineContext) -> Result<(), &'static str>,
    {
        if self.need_cache(new_p) {
            bind(self)?;
            *T::get_cache(self) = Rc::downgrade(new_p);
        }

        Ok(())
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

impl<A> Engine<A>
where
    A: AssetSystem,
{
    pub fn clear(&self) {
        self.gl.clear(BufferBit::Color);
        self.gl.clear(BufferBit::Depth);
        self.gl.clear_color(0.2, 0.2, 0.2, 1.0);
    }

    fn setup_material(&self, ctx: &mut EngineContext, material: &Material) -> Result<(), &str> {
        ctx.prepare_cache(&material.program, |ctx| {
            material.program.bind(&self.gl);
            ctx.switch_prog += 1;
            Ok(())
        })?;

        ctx.prepare_cache(&material.texture, |ctx| {
            let curr = ctx.prog.upgrade().unwrap();
            // Binding texture
            if !material.texture.bind(&self.gl, &curr) {
                return Err("Texture is not ready.");
            }
            ctx.switch_tex += 1;
            Ok(())
        })?;

        if let Some(ref prog) = ctx.prog.upgrade() {
            // temp set the material shiness here
            prog.set("uShininess", 32.0);
            self.setup_light(ctx, &prog);
        }

        Ok(())
    }

    fn setup_camera(&self, ctx: &mut EngineContext, object: &GameObject, camera: &Camera) {
        // Setup Matrices
        let mut modelm = object.transform.to_homogeneous();
        modelm = modelm * Matrix4::new_nonuniform_scaling(&object.scale);

        let prog = ctx.prog.upgrade().unwrap();
        // setup_camera
        prog.set("uMVMatrix", camera.v * modelm);
        prog.set("uPMatrix", camera.p);
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

    fn render_object(
        &self,
        gl: &WebGLRenderingContext,
        ctx: &mut EngineContext,
        object: &GameObject,
        camera: &Camera,
    ) {
        self.setup_camera(ctx, object, camera);

        // Setup Mesh
        object.find_component::<Mesh>().map(|(mesh_ref, _)| {
            let prog = ctx.prog.upgrade().unwrap();

            let mesh = mesh_ref.borrow();

            ctx.prepare_cache(&mesh.mesh_buffer, |ctx| {
                mesh.bind(&self.gl, &prog);
                ctx.switch_mesh += 1;
                Ok(())
            }).unwrap();

            prog.commit(gl);
            mesh.render(gl);
        });
    }

    pub fn begin(&mut self) {
        imgui::begin();
    }

    pub fn end(&mut self) {}

    fn map_component<T, F>(&self, mut func: F)
    where
        T: 'static + ComponentBased,
        F: FnMut(Arc<Component>) -> bool,
    {
        for obj in self.objects.iter() {
            if let Some(r) = obj.upgrade().map_or(None, |obj| {
                obj.borrow().find_component::<T>().map(|(_, c)| c)
            }) {
                if !func(r) {
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

    pub fn render(&mut self) {
        self.clear();
        imgui::pre_render(self);

        let gl = &self.gl;
        let objects = &self.objects;
        if let &Some(camera) = &self.main_camera.as_ref() {
            let mut ctx: EngineContext = Default::default();

            // prepare main light.
            ctx.main_light = Some(self.find_component::<Light>().unwrap_or({
                Component::new(Light::Directional(DirectionalLight {
                    direction: Vector3::new(0.5, -1.0, 1.0).normalize(),
                    ambient: Vector3::new(0.2, 0.2, 0.2),
                    diffuse: Vector3::new(0.5, 0.5, 0.5),
                    specular: Vector3::new(1.0, 1.0, 1.0),
                }))
            }));

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

            for obj in objects.iter() {
                obj.upgrade().map(|obj| {
                    let object = obj.borrow();
                    if let Some((material_ref, _)) = object.find_component::<Material>() {
                        let material = material_ref.borrow();

                        if self.setup_material(&mut ctx, &material).is_ok() {
                            self.render_object(gl, &mut ctx, &object, camera);
                        }
                    }
                });
            }
        }

        // drop all gameobjects if there are no other references
        self.objects.retain(|obj| obj.upgrade().is_some());
    }

    pub fn new(app: &App, size: (u32, u32)) -> Engine<A> {
        let gl = WebGLRenderingContext::new(app.canvas());

        /*=========Drawing the triangle===========*/

        // Clear the canvas
        gl.clear_color(0.5, 0.5, 0.5, 1.0);

        // Enable the depth test
        gl.enable(Flag::DepthTest);

        // Enable alpha blending
        gl.enable(Flag::Blend);

        // Clear the color buffer bit
        gl.clear(BufferBit::Color);
        gl.clear(BufferBit::Depth);
        gl.blend_func(BlendMode::SrcAlpha, BlendMode::OneMinusSrcAlpha);

        // Set the view port
        gl.viewport(0, 0, size.0, size.1);

        Engine {
            gl: gl,
            main_camera: None,
            objects: vec![],
            program_cache: RefCell::new(HashMap::new()),
            asset_system: Rc::new(A::new()),
            gui_context: Rc::new(RefCell::new(imgui::Context::new(size.0, size.1))),
        }
    }
}

impl<A: AssetSystem> IEngine for Engine<A> {
    fn new_gameobject(&mut self) -> Rc<RefCell<GameObject>> {
        let go = Rc::new(RefCell::new(GameObject {
            transform: Isometry3::identity(),
            scale: Vector3::new(1.0, 1.0, 1.0),
            components: vec![],
        }));

        self.objects.push(Rc::downgrade(&go));
        go
    }

    fn gui_context(&mut self) -> Rc<RefCell<imgui::Context>> {
        self.gui_context.clone()
    }

    fn asset_system<'a>(&'a self) -> &'a AssetSystem {
        &*self.asset_system
    }
}