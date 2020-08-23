use crate::datatype::{Dimension, Position, ChunkUnit, BlockUnit};
use crate::world::mesh::{Mesh, MeshType};
use crate::world::chunk::{Chunk, CHUNK_SIZE};
use crate::world::shader::{CubeVert, cube_vs, cube_fs};
use crate::world::ChunkID;
use crate::world::block::Block;
use crate::world::player::Player;
use crate::event::types::ChunkEvents;

use vulkano::pipeline::viewport::Viewport;
use vulkano::framebuffer::{Subpass, RenderPassAbstract};
use vulkano::device::Device;
use vulkano::pipeline::{GraphicsPipelineAbstract, GraphicsPipeline};
use vulkano::command_buffer::DynamicState;
use vulkano::buffer::{CpuAccessibleBuffer, BufferUsage, CpuBufferPool};
use vulkano::sampler::{Sampler, SamplerAddressMode, Filter, MipmapMode};
use vulkano::descriptor::descriptor_set::{PersistentDescriptorSet};
use vulkano::descriptor::DescriptorSet;
use vulkano::buffer::cpu_pool::CpuBufferPoolSubbuffer;
use vulkano::memory::pool::StdMemoryPool;
use vulkano::image::ImmutableImage;
use vulkano::format::Format;

use rayon::prelude::*;

use std::sync::Arc;
use std::iter;
use std::marker::PhantomData;
use crate::world::chunk_threadpool::{ChunkThreadPool, ThreadPoolOutput};


pub enum Side {
    Top,
    Bottom,
    Left,
    Right,
    Front,
    Back,
}

// only stores an immutable reference to a vector of meshes
// the cube mesh will only re-render the render data when the is update
pub struct Cube {
    textures: Arc<ImmutableImage<Format>>,
    // chunks: Chunk Reference, Chunk Cullling, Chunk Vertices, Chunk Indices
    chunks: Vec<(ChunkID, bool, Vec<<Self as Mesh>::Vertex>, Vec<<Self as Mesh>::Index>)>,
    grph_pipe: Arc<dyn GraphicsPipelineAbstract + Send + Sync>,

    vert_shd: cube_vs::Shader,
    frag_shd: cube_fs::Shader,

    persp_mat: CpuBufferPool<cube_vs::ty::MVP>,
    persp_buf: Option<CpuBufferPoolSubbuffer<cube_vs::ty::MVP, Arc<StdMemoryPool>>>,
    sampler: Arc<Sampler>,

    vertices: Vec<<Self as Mesh>::Vertex>,  // aggregated vertices (stored to optimize)
    indices: Vec<<Self as Mesh>::Index>,  // aggregated indices (stored to optimize)
}

impl Cube {
    pub fn new(device: Arc<Device>,
               texture: Arc<ImmutableImage<Format>>,
               render_pass: Arc<dyn RenderPassAbstract + Send + Sync>,
               dimensions: Dimension<u32>
    ) -> Self {
        let vs = cube_vs::Shader::load(device.clone()).expect("failed to create cube vertex shaders module");
        let fs = cube_fs::Shader::load(device.clone()).expect("failed to create cube fragment shaders module");

        Self {
            textures: texture.clone(),
            chunks: Vec::new(),
            grph_pipe: Cube::pipeline(
                &vs, &fs, device.clone(),
                render_pass.clone(), dimensions.into(),
            ),

            vert_shd: vs,
            frag_shd: fs,

            persp_mat: CpuBufferPool::uniform_buffer(device.clone()),
            persp_buf: None,
            sampler: Sampler::new(device.clone(), Filter::Nearest, Filter::Nearest,
                                  MipmapMode::Nearest, SamplerAddressMode::Repeat, SamplerAddressMode::Repeat,
                                  SamplerAddressMode::Repeat, 0.0, 1.0, 0.0, 8.0).unwrap(),

            vertices: Vec::new(),
            indices: Vec::new(),
        }
    }

    // internal function for building pipeline
    fn pipeline(
        vert: &cube_vs::Shader,
        frag: &cube_fs::Shader,
        device: Arc<Device>,
        render_pass: Arc<dyn RenderPassAbstract + Send + Sync>,
        dimensions: Dimension<u32> )
        -> Arc<dyn GraphicsPipelineAbstract + Send + Sync> {

        // specialization constants will not be used
        // let spec_consts = cube_fs::SpecializationConstants {
        //     TEXTURES: texture.texture_len() as u32
        // };

        Arc::new(GraphicsPipeline::start()
            .vertex_input_single_buffer::<<Cube as Mesh>::Vertex>()
            .vertex_shader(vert.main_entry_point(), ())
            .triangle_list()
            .viewports_dynamic_scissors_irrelevant(1)
            .viewports(iter::once(Viewport {
                origin: [0.0, 0.0],
                dimensions: dimensions.into(),
                depth_range: 0.0 .. 1.0,
            }))
            .fragment_shader(frag.main_entry_point(), ())
            .cull_mode_front()  // face culling for optimization TODO: make it changeable
            .alpha_to_coverage_enabled()  // to enable transparency
            .depth_stencil_simple_depth()  // to enable depth buffering
            .render_pass(Subpass::from(render_pass.clone(), 0).unwrap())
            .build(device.clone()).unwrap()
        )
    }

    fn mesh_data(chunk_list: Vec<(ChunkID, bool, Vec<CubeVert>, Vec<u32>)>, chunks: Arc<Vec<Chunk>>, chunk: Chunk) -> ThreadPoolOutput {
        let find_chunk = |cid: ChunkID| -> Option<&Chunk> {
            // don't parallelize this iterator: as this closure gets executed few thousands time,
            // the overhead of even threadpool can largely affect negatively
            if let Some(ind) = chunks.iter().position(|x| x.id == cid) {
                Some(&chunks[ind])
            } else {
                None
            }
        };

        let start = Position::new(
            chunk.position.x.into_block(),
            chunk.position.y.into_block(),
            chunk.position.z.into_block(),
        );

        let end = Position::new(
            (chunk.position.x + ChunkUnit(1.0)).into_block() - BlockUnit(1.0),
            (chunk.position.y + ChunkUnit(1.0)).into_block() - BlockUnit(1.0),
            (chunk.position.z + ChunkUnit(1.0)).into_block() - BlockUnit(1.0),
        );

        // println!("Chunk Start: {:?}", start);
        // println!("Chunk End: {:?}", end);

        // Chunk Border Culling:
        // first locate any adjacent chunks in the world.mesh
        // then locate the world.block data to find any transparent blocks
        //  ^- IF NOT: do not add the vertices and indices
        //  ^- ELSE: add the vertices and indices to the vector
        //  ^- CASE: when there are no adjacent chunks: do not add the vertices and indices

        // listing out all the theoretical adjacent chunks this Chunk has
        let adjc_chunks: [Position<ChunkUnit>; 6] = [
            Position::new(chunk.position.x+ChunkUnit(1.0), chunk.position.y  , chunk.position.z  ),  // LEFT
            Position::new(chunk.position.x-ChunkUnit(1.0), chunk.position.y  , chunk.position.z  ),  // RIGHT
            Position::new(chunk.position.x  , chunk.position.y+ChunkUnit(1.0), chunk.position.z  ),  // UP
            Position::new(chunk.position.x  , chunk.position.y-ChunkUnit(1.0), chunk.position.z  ),  // DOWN
            Position::new(chunk.position.x  , chunk.position.y  , chunk.position.z+ChunkUnit(1.0)),  // BACK
            Position::new(chunk.position.x  , chunk.position.y  , chunk.position.z-ChunkUnit(1.0)),  // FRONT
        ];

        // lists all the chunks that are adjacent to this Chunk
        let merge_chunks = chunk_list.par_iter()
            .filter(|c| {
                if let Some(c) = &find_chunk(c.0) {
                    adjc_chunks.contains(&c.position)
                } else {
                    false
                }
            })
            .collect::<Vec<_>>();

        // println!("Merge Chunks: {:?}", merge_chunks.iter().map(|c| c.0).collect::<Vec<_>>());

        let main_chunk = chunk.clone();

        // the coordinates of get_chunk(); coords relative to the main Chunk
        let get_chunk = |x, y, z| {
            for (cid, _cull, _v, _i) in &merge_chunks {
                if let Some(c_chunk) = &find_chunk(*cid) {
                    let offset = Position::new(
                        c_chunk.position.x - main_chunk.position.x,
                        c_chunk.position.y - main_chunk.position.y,
                        c_chunk.position.z - main_chunk.position.z,
                    );

                    if Position::new(x, y, z) == offset {
                        return Some(c_chunk.clone());
                    }
                }
            }
            return None;
        };

        // println!("Current chunk position: {:?}", chunk.position);
        // println!("Adjacent chunks found: {:?}", merge_chunks.iter().map(|x| &find_chunk(&x.0).unwrap().position).collect::<Vec<_>>());

        let mut vertices = Vec::new();
        let mut indices = Vec::new();

        // NOTE: FOR THE HECK SAKE, the bug was I have to change the u32 to i32 because of negative position. sigh.
        for x in start.x.into()..=i32::from(end.x) {
            let x = BlockUnit(x as f32);
            for y in start.y.into()..=i32::from(end.y) {
                let y = BlockUnit(y as f32);
                for z in start.z.into()..=i32::from(end.z) {
                    let z = BlockUnit(z as f32);
                    let block: &Block = chunk.blocks(x, y, z);

                    /*
                        1 -- 3
                        | \  |
                        |  \ |
                        0 -- 2
                     */

                    if let MeshType::Cube {top, bottom, left, right, front, back} = &block.mesh {
                        let mut faces = 0;

                        // if if (1st: checks chunk border) {2nd: checks for nearby transparent world.block across the chunk border} else {3rd: checks for nearby transparent world.block}
                        if  if start.x == x {
                            if let Some(c) = get_chunk(ChunkUnit(-1.0), ChunkUnit(0.0), ChunkUnit(0.0)) {
                                c.blocks(BlockUnit(CHUNK_SIZE as f32-1.0), y, z).state.transparent && !block.state.transparent
                            } else {
                                false
                            }
                        } else {
                            chunk.blocks(x.decr(), y, z).state.transparent && !block.state.transparent
                        }
                        {  // left face
                            vertices.push(CubeVert { pos: [0.0+x.inner(),0.0+y.inner(),1.0+z.inner()], txtr: 1 | (12 << 2) | (left.0 << 16)});
                            vertices.push(CubeVert { pos: [0.0+x.inner(),1.0+y.inner(),1.0+z.inner()], txtr: 0 | (12 << 2) | (left.0 << 16)});
                            vertices.push(CubeVert { pos: [0.0+x.inner(),1.0+y.inner(),0.0+z.inner()], txtr: 2 | (12 << 2) | (left.0 << 16)});
                            vertices.push(CubeVert { pos: [0.0+x.inner(),0.0+y.inner(),0.0+z.inner()], txtr: 3 | (12 << 2) | (left.0 << 16)});
                            faces += 1;
                        }
                        if if start.y == y {
                            if let Some(c) = get_chunk(ChunkUnit(0.0), ChunkUnit(-1.0), ChunkUnit(0.0)) {
                                c.blocks(x, BlockUnit(CHUNK_SIZE as f32-1.0), z).state.transparent && !block.state.transparent
                            } else {
                                false
                            }
                        } else {
                            chunk.blocks(x, y.decr(), z).state.transparent && !block.state.transparent
                        }
                        {  // bottom face
                            vertices.push(CubeVert { pos: [0.0+x.inner(), 0.0+y.inner(), 0.0+z.inner()], txtr: 0 | (8 << 2) | (bottom.0 << 16)});
                            vertices.push(CubeVert { pos: [1.0+x.inner(), 0.0+y.inner(), 0.0+z.inner()], txtr: 2 | (8 << 2) | (bottom.0 << 16)});
                            vertices.push(CubeVert { pos: [1.0+x.inner(), 0.0+y.inner(), 1.0+z.inner()], txtr: 3 | (8 << 2) | (bottom.0 << 16)});
                            vertices.push(CubeVert { pos: [0.0+x.inner(), 0.0+y.inner(), 1.0+z.inner()], txtr: 1 | (8 << 2) | (bottom.0 << 16)});
                            faces += 1;
                        }
                        if if start.z == z {
                            if let Some(c) = get_chunk(ChunkUnit(0.0), ChunkUnit(0.0), ChunkUnit(-1.0)) {
                                c.blocks(x, y, BlockUnit(CHUNK_SIZE as f32-1.0)).state.transparent && !block.state.transparent
                            } else {
                                false
                            }
                        } else {
                            chunk.blocks(x, y, z.decr()).state.transparent && !block.state.transparent
                        }
                        {  // front face
                            vertices.push(CubeVert { pos: [0.0+x.inner(), 1.0+y.inner(), 0.0+z.inner()], txtr: 0 | (12 << 2) | (front.0 << 16)});
                            vertices.push(CubeVert { pos: [1.0+x.inner(), 1.0+y.inner(), 0.0+z.inner()], txtr: 2 | (12 << 2) | (front.0 << 16)});
                            vertices.push(CubeVert { pos: [1.0+x.inner(), 0.0+y.inner(), 0.0+z.inner()], txtr: 3 | (12 << 2) | (front.0 << 16)});
                            vertices.push(CubeVert { pos: [0.0+x.inner(), 0.0+y.inner(), 0.0+z.inner()], txtr: 1 | (12 << 2) | (front.0 << 16)});
                            faces += 1;
                        }
                        if if end.x == x {
                            if let Some(c) = get_chunk(ChunkUnit(1.0), ChunkUnit(0.0), ChunkUnit(0.0)) {
                                c.blocks(BlockUnit(0.0), y, z).state.transparent && !block.state.transparent
                            } else {
                                false
                            }
                        } else {
                            chunk.blocks(x.incr(), y, z).state.transparent && !block.state.transparent
                        }
                        {  // right face
                            vertices.push(CubeVert { pos: [1.0+x.inner(), 0.0+y.inner(), 0.0+z.inner()], txtr: 3 | (12 << 2) | (right.0 << 16)});
                            vertices.push(CubeVert { pos: [1.0+x.inner(), 1.0+y.inner(), 0.0+z.inner()], txtr: 2 | (12 << 2) | (right.0 << 16)});
                            vertices.push(CubeVert { pos: [1.0+x.inner(), 1.0+y.inner(), 1.0+z.inner()], txtr: 0 | (12 << 2) | (right.0 << 16)});
                            vertices.push(CubeVert { pos: [1.0+x.inner(), 0.0+y.inner(), 1.0+z.inner()], txtr: 1 | (12 << 2) | (right.0 << 16)});
                            faces += 1;
                        }
                        if if end.y == y {
                            if let Some(c) = get_chunk(ChunkUnit(0.0), ChunkUnit(1.0), ChunkUnit(0.0)) {
                                c.blocks(x, BlockUnit(0.0), z).state.transparent && !block.state.transparent
                            } else {
                                false
                            }
                        } else {
                            chunk.blocks(x, y.incr(), z).state.transparent && !block.state.transparent
                        }
                        {  // top face
                            vertices.push(CubeVert { pos: [0.0+x.inner(), 1.0+y.inner(), 1.0+z.inner()], txtr: 0 | (15 << 2) | (top.0 << 16)});
                            vertices.push(CubeVert { pos: [1.0+x.inner(), 1.0+y.inner(), 1.0+z.inner()], txtr: 1 | (15 << 2) | (top.0 << 16)});
                            vertices.push(CubeVert { pos: [1.0+x.inner(), 1.0+y.inner(), 0.0+z.inner()], txtr: 3 | (15 << 2) | (top.0 << 16)});
                            vertices.push(CubeVert { pos: [0.0+x.inner(), 1.0+y.inner(), 0.0+z.inner()], txtr: 2 | (15 << 2) | (top.0 << 16)});
                            faces += 1;
                        }
                        if if end.z == z {
                            if let Some(c) = get_chunk(ChunkUnit(0.0), ChunkUnit(0.0), ChunkUnit(1.0)) {
                                c.blocks(x, y, BlockUnit(0.0)).state.transparent && !block.state.transparent
                            } else {
                                false
                            }
                        } else {
                            chunk.blocks(x, y, z.incr()).state.transparent && !block.state.transparent
                        }
                        {  // back face
                            vertices.push(CubeVert { pos: [0.0+x.inner(),0.0+y.inner(),1.0+z.inner()], txtr: 3 | (12 << 2) | (back.0 << 16)});
                            vertices.push(CubeVert { pos: [1.0+x.inner(),0.0+y.inner(),1.0+z.inner()], txtr: 1 | (12 << 2) | (back.0 << 16)});
                            vertices.push(CubeVert { pos: [1.0+x.inner(),1.0+y.inner(),1.0+z.inner()], txtr: 0 | (12 << 2) | (back.0 << 16)});
                            vertices.push(CubeVert { pos: [0.0+x.inner(),1.0+y.inner(),1.0+z.inner()], txtr: 2 | (12 << 2) | (back.0 << 16)});
                            faces += 1;
                        }

                        if indices.is_empty() {
                            if faces > 0 {
                                indices.append(
                                    &mut vec![
                                        0, 1, 2,
                                        0, 2, 3,
                                    ]
                                );
                                faces -= 1;
                            }
                        }

                        for _ in 0..faces {
                            let ofs = *indices.last().unwrap() as u32+1;  // offset
                            indices.append(
                                &mut vec![
                                    0+ofs, 1+ofs, 2+ofs,  // triangle 1
                                    0+ofs, 2+ofs, 3+ofs,  // triangle 2
                                ]
                            )
                        }
                    }
                }
            }
        }

        (Box::new(vertices), Box::new(indices))
    }
}

impl Mesh for Cube {
    type Vertex = CubeVert;
    type Index = u32;

    type PushConstants = ();

    fn add_chunk(&mut self, chunk_id: ChunkID) {
        // ( chunk reference, vertices vector, indices vector )
        self.chunks.push((chunk_id, false, Vec::new(), Vec::new()));
    }

    fn load_chunks(&mut self, chunks: Vec<Chunk>, pool: &mut ChunkThreadPool) {
        let mut chunk_cloned = self.chunks.clone();
        let chunks = Arc::new(chunks);

        for (chunk_id, _cull, _vert, _indx) in chunk_cloned.iter_mut() {
            // println!("Chunk loop");
            // println!("Chunks Arc Ref: {}", Arc::strong_count(&chunks));

            let cloned_chunks = chunks.clone();

            let find_chunk = |cid: ChunkID| -> Option<Chunk> {
                if let Some(ind) = cloned_chunks.par_iter().position_first(|x| x.id == cid) {
                    Some(cloned_chunks[ind].clone())
                } else {
                    None
                }
            };

            if let Some(chunk) = find_chunk(*chunk_id) {
                let chunk_list = self.chunks.clone();
                let chunks = chunks.clone();

                // println!("Adding a chunk thread");
                let chunk = chunk.clone();
                pool.add_work( ( chunk_id.clone(), Box::new(move || {
                    Self::mesh_data(chunk_list, chunks, chunk)
                })));  // end for adding work to the thread pool
            }
        }

        let output = pool.join();

        for (id, (mut vert, mut indx)) in output {
            // as long the self.chunks doesn't get changed in between, it should never panic
            let ind = self.chunks.iter().position(|c| c.0 == id).unwrap();

            let mut vertices: &mut Vec<CubeVert> = (*vert).downcast_mut().unwrap();
            let mut indices: &mut Vec<u32> = (*indx).downcast_mut().unwrap();

            // .2: vertex dt of that chunk; .3 index dt of that chunk

            // just in case if there are any vertices/indices data this chunk has previously
            // which can cause some rendering issues
            self.chunks[ind].2.clear();
            self.chunks[ind].3.clear();

            self.chunks[ind].2.append(&mut vertices);
            self.chunks[ind].3.append(&mut indices);
        }


    }

    // TODO: Will probably be removed in future
    // re-evaluates the vertex and index data buffer
    fn updt_chunks(&mut self, id: ChunkID) {
        unimplemented!()
    }

    // removes the buffer and its reference
    fn remv_chunk(&mut self, id: ChunkID) {
        for ind in 0..self.chunks.len() {
            if self.chunks[ind].0 == id {
                self.chunks.swap_remove(ind);
                break;
            }
        }
    }

    fn updt_world(&mut self, dimensions: Dimension<u32>, player: &Player) {
        let (proj, view, world) = player.camera.gen_mvp(dimensions);

        self.persp_buf = Some(self.persp_mat.next(
            cube_vs::ty::MVP {proj: proj, view: view, world: world}
        ).unwrap());

        // TODO: frustum culling

        // let _ = world.player.camera.frustum(dimensions);

        // for chunk in self.chunks {
        //     if chunk.0.position {
        //
        //     }
        // }
    }

    // renders the buffers and pipeline; only merges the vertex and index data into a one large buffer
    // called for each frame
    fn render<'b>(&mut self,
                  device: Arc<Device>,
                  renderpass: Arc<dyn RenderPassAbstract + Send + Sync>,
                  dimensions: Dimension<u32>,
                  rerender: bool,
                  chunk_event: Vec<ChunkEvents>,
    ) -> (
            Arc<dyn GraphicsPipelineAbstract + Send + Sync>,  // graphic pipeline
            DynamicState,  // dynamic state for display
            Arc<CpuAccessibleBuffer<[Self::Vertex]>>,   // vertex buffer
            Arc<CpuAccessibleBuffer<[Self::Index]>>,  // index buffer
            Vec<Arc<dyn DescriptorSet+Send+Sync+'b>>,   // sets (aka uniforms) buffer
            Self::PushConstants,   // push-down constants
    ) {
        if !chunk_event.is_empty() {
            // println!("Mesh Rendering: Chunk Status ({:?})", chunk_event);
        }

        if rerender {
            self.grph_pipe = Self::pipeline(
                &self.vert_shd, &self.frag_shd,
                device.clone(), renderpass.clone(), dimensions
            );
        }

        if  self.vertices.is_empty() ||
            self.indices.is_empty() ||
            chunk_event.contains(&ChunkEvents::ReloadChunks) ||
            chunk_event.iter().any(|e| if let ChunkEvents::LoadChunk(_) = e {true} else {false})
        {
            self.vertices.clear();
            self.indices.clear();

            for (_chunk, cull, vertices, _indices) in self.chunks.iter() {
                if !*cull {  // check if the chunk is visible to be loaded (using frustum culling)
                    self.vertices.extend(vertices.iter());
                }
            }

            for (_chunk, cull, _vertices, indices) in self.chunks.iter() {
                if !*cull {
                    if self.indices.is_empty() {
                        self.indices.extend(
                            indices.iter()
                        );
                    } else {
                        // last number of the index is always that largest
                        // IF using the format: 0 1 2 0 2 3
                        let indx_max = self.indices.last().unwrap().clone();
                        self.indices.extend(
                            indices.iter().map(|&x| x+indx_max+1)
                        );
                    }
                }
            }

            // println!("Vertex length: {:?}", self.vertices.len());
            // println!("Index length: {:?}", self.indices.len());
        }

        // TODO: Dynamically add new texture buffers
        let layout0 = self.grph_pipe.descriptor_set_layout(0).unwrap();
        let set0 = Arc::new(PersistentDescriptorSet::start(layout0.clone())
            .add_sampled_image(self.textures.clone(), self.sampler.clone()).unwrap()
            .build().unwrap()
        );

        let layout1 = self.grph_pipe.descriptor_set_layout(1).unwrap();
        let set1 = Arc::new(PersistentDescriptorSet::start(layout1.clone())
            .add_buffer(self.persp_buf.as_ref().unwrap().clone()).unwrap()
            .build().unwrap()
        );

        (
            self.grph_pipe.clone(),
            DynamicState::none(),
            CpuAccessibleBuffer::from_iter(
                device.clone(), BufferUsage::vertex_buffer(),
                false, self.vertices.clone().into_iter()
            ).unwrap(),
            CpuAccessibleBuffer::from_iter(
                device.clone(), BufferUsage::index_buffer(),
                false, self.indices.clone().into_iter()
            ).unwrap(),
            vec![set0, set1],
            (),
        )
    }
}
