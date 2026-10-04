//! API-agnostic draw data.
//!
//! UI backends fill a [`DrawData`] every frame; renderers (DX11, DX12) only
//! ever see this type. The vertex layout is the one egui and Dear ImGui
//! already use (`pos: [f32; 2]`, `uv: [f32; 2]`, `rgba8`), so converting is a
//! plain copy.

use std::collections::HashMap;

/// One vertex. 20 bytes, `#[repr(C)]`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Vertex {
    /// Position in [`DrawData::display_size`] units (top-left origin).
    pub pos: [f32; 2],
    /// Texture coordinates.
    pub uv: [f32; 2],
    /// RGBA8 vertex colour (premultiplied or not, see [`BlendMode`]).
    pub color: [u8; 4],
}

/// Texture handle chosen by the UI backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TextureId(pub u64);

/// How vertex colours and texels are blended into the back buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum BlendMode {
    /// Premultiplied alpha (`ONE, INV_SRC_ALPHA`), used by egui.
    #[default]
    Premultiplied,
    /// Straight alpha (`SRC_ALPHA, INV_SRC_ALPHA`), used by Dear ImGui.
    Straight,
}

/// Texture sampling filter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Filter {
    /// Bilinear.
    #[default]
    Linear,
    /// Point sampling.
    Nearest,
}

/// Creates a texture, replaces it, or updates a sub-rectangle of it.
#[derive(Clone, Debug)]
pub struct TextureUpdate {
    /// Texture to create / update.
    pub id: TextureId,
    /// `None` = full (re)creation, `Some([x, y])` = sub-rectangle update.
    pub offset: Option<[u32; 2]>,
    /// Width and height of `pixels`.
    pub size: [u32; 2],
    /// Tightly packed RGBA8 pixels, `size[0] * size[1] * 4` bytes.
    pub pixels: Vec<u8>,
    /// Sampling filter.
    pub filter: Filter,
}

/// One indexed draw call.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DrawCmd {
    /// Scissor rectangle in back-buffer **pixels**: `[x0, y0, x1, y1]`.
    pub clip: [f32; 4],
    /// Texture sampled by this draw.
    pub texture: TextureId,
    /// First index in [`DrawData::indices`].
    pub idx_offset: u32,
    /// Number of indices.
    pub idx_count: u32,
    /// Added to every index of this draw.
    pub vtx_offset: u32,
}

/// Everything the renderer needs for one frame.
#[derive(Clone, Debug, Default)]
pub struct DrawData {
    /// Size of the coordinate space of [`Vertex::pos`] (points for egui,
    /// pixels for imgui). Mapped onto the whole back buffer.
    pub display_size: [f32; 2],
    /// Blend mode of this frame.
    pub blend: BlendMode,
    /// All vertices of the frame.
    pub vertices: Vec<Vertex>,
    /// All indices of the frame.
    pub indices: Vec<u32>,
    /// Draw calls, in order.
    pub cmds: Vec<DrawCmd>,
    /// Texture creations / updates, applied before drawing.
    pub texture_updates: Vec<TextureUpdate>,
    /// Textures to free, applied after drawing.
    pub texture_frees: Vec<TextureId>,
}

impl DrawData {
    /// Clears everything but keeps the allocations.
    pub fn clear(&mut self) {
        self.vertices.clear();
        self.indices.clear();
        self.cmds.clear();
        self.texture_updates.clear();
        self.texture_frees.clear();
    }

    /// Appends one mesh as one draw call (indices are relative to `vertices`).
    pub fn push_mesh(
        &mut self,
        vertices: &[Vertex],
        indices: impl IntoIterator<Item = u32>,
        clip: [f32; 4],
        texture: TextureId,
    ) {
        let vtx_offset = self.vertices.len() as u32;
        let idx_offset = self.indices.len() as u32;
        self.vertices.extend_from_slice(vertices);
        self.indices.extend(indices);
        let idx_count = self.indices.len() as u32 - idx_offset;
        if idx_count != 0 {
            self.cmds.push(DrawCmd {
                clip,
                texture,
                idx_offset,
                idx_count,
                vtx_offset,
            });
        }
    }
}

/// CPU copy of every live texture. When the renderer is re-created (new
/// device / swap chain) all textures are replayed, so UI backends never have
/// to know that the GPU resources were lost.
#[derive(Default)]
pub(crate) struct TextureMirror {
    map: HashMap<TextureId, TextureUpdate>,
}

impl TextureMirror {
    pub fn apply(&mut self, updates: &[TextureUpdate], frees: &[TextureId]) {
        for u in updates {
            match u.offset {
                None => {
                    self.map.insert(u.id, u.clone());
                }
                Some([x, y]) => {
                    let Some(full) = self.map.get_mut(&u.id) else { continue };
                    full.filter = u.filter;
                    let [fw, fh] = full.size;
                    for row in 0..u.size[1] {
                        let dy = y + row;
                        if dy >= fh {
                            break;
                        }
                        let w = u.size[0].min(fw.saturating_sub(x)) as usize;
                        let src = (row * u.size[0]) as usize * 4;
                        let dst = (dy * fw + x) as usize * 4;
                        if w == 0 || src + w * 4 > u.pixels.len() {
                            break;
                        }
                        full.pixels[dst..dst + w * 4].copy_from_slice(&u.pixels[src..src + w * 4]);
                    }
                }
            }
        }
        for id in frees {
            self.map.remove(id);
        }
    }

    /// Full uploads of every texture (after a renderer re-creation).
    pub fn replay(&self) -> Vec<TextureUpdate> {
        self.map.values().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full(id: u64, w: u32, h: u32, v: u8) -> TextureUpdate {
        TextureUpdate {
            id: TextureId(id),
            offset: None,
            size: [w, h],
            pixels: vec![v; (w * h * 4) as usize],
            filter: Filter::Linear,
        }
    }

    #[test]
    fn push_mesh_offsets() {
        let mut d = DrawData::default();
        let v = [Vertex::default(); 3];
        d.push_mesh(&v, [0, 1, 2], [0.0; 4], TextureId(1));
        d.push_mesh(&v, [0, 1, 2], [0.0; 4], TextureId(2));
        d.push_mesh(&v, [], [0.0; 4], TextureId(3)); // empty: no draw call
        assert_eq!(d.cmds.len(), 2);
        assert_eq!(d.cmds[1].vtx_offset, 3);
        assert_eq!(d.cmds[1].idx_offset, 3);
        assert_eq!(d.cmds[1].idx_count, 3);
    }

    #[test]
    fn mirror_partial_and_free() {
        let mut m = TextureMirror::default();
        m.apply(&[full(1, 4, 4, 0)], &[]);
        let patch = TextureUpdate {
            offset: Some([2, 2]),
            ..full(1, 2, 2, 9)
        };
        m.apply(&[patch], &[]);
        let t = &m.replay()[0];
        let px = |x: u32, y: u32| t.pixels[((y * 4 + x) * 4) as usize];
        assert_eq!((px(1, 1), px(2, 2), px(3, 3)), (0, 9, 9));
        m.apply(&[], &[TextureId(1)]);
        assert!(m.replay().is_empty());
    }

    #[test]
    fn mirror_partial_out_of_bounds_is_clipped() {
        let mut m = TextureMirror::default();
        m.apply(&[full(1, 2, 2, 0)], &[]);
        m.apply(
            &[TextureUpdate {
                offset: Some([1, 1]),
                ..full(1, 4, 4, 7)
            }],
            &[],
        );
        assert_eq!(m.replay()[0].pixels.len(), 16);
    }
}
