//! Texture atlases for rasterized glyphs and icons, packed into horizontal shelves.

use std::collections::HashMap;
use std::hash::Hash;

pub const ATLAS_SIZE: u32 = 2048;
const PADDING: u32 = 1;

#[derive(Clone, Copy, Debug)]
pub struct Slot {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

struct Shelf {
    y: u32,
    height: u32,
    cursor: u32,
}

pub struct Atlas<K> {
    pub texture: wgpu::Texture,
    format: wgpu::TextureFormat,
    queue: wgpu::Queue,
    shelves: Vec<Shelf>,
    entries: HashMap<K, Option<(Slot, [i32; 2])>>,
    /// Set when the atlas filled up and was reset; callers should redraw.
    pub overflowed: bool,
}

impl<K: Hash + Eq + Clone> Atlas<K> {
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue, format: wgpu::TextureFormat, label: &str) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d { width: ATLAS_SIZE, height: ATLAS_SIZE, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        Self {
            texture,
            format,
            queue: queue.clone(),
            shelves: Vec::new(),
            entries: HashMap::new(),
            overflowed: false,
        }
    }

    /// Returns the slot and bearing (left, top) for `key`, rasterizing it with `raster` on a miss.
    /// `raster` returns `(width, height, left, top, pixels)` or `None` for empty glyphs.
    pub fn get_or_insert(
        &mut self,
        key: &K,
        raster: impl FnOnce() -> Option<(u32, u32, i32, i32, Vec<u8>)>,
    ) -> Option<(Slot, [i32; 2])> {
        if let Some(entry) = self.entries.get(key) {
            return *entry;
        }
        let entry = raster().and_then(|(w, h, left, top, data)| {
            if w == 0 || h == 0 {
                return None;
            }
            let slot = match self.allocate(w, h) {
                Some(slot) => slot,
                None => {
                    self.clear();
                    self.overflowed = true;
                    self.allocate(w, h)?
                }
            };
            self.upload(slot, &data);
            Some((slot, [left, top]))
        });
        self.entries.insert(key.clone(), entry);
        entry
    }

    fn clear(&mut self) {
        self.shelves.clear();
        self.entries.clear();
    }

    fn allocate(&mut self, w: u32, h: u32) -> Option<Slot> {
        let (pw, ph) = (w + PADDING, h + PADDING);
        for shelf in &mut self.shelves {
            if ph <= shelf.height && shelf.height <= ph + ph / 2 + 2 && shelf.cursor + pw <= ATLAS_SIZE {
                let slot = Slot { x: shelf.cursor, y: shelf.y, w, h };
                shelf.cursor += pw;
                return Some(slot);
            }
        }
        let y = self.shelves.last().map_or(0, |s| s.y + s.height);
        if y + ph > ATLAS_SIZE || pw > ATLAS_SIZE {
            return None;
        }
        self.shelves.push(Shelf { y, height: ph, cursor: pw });
        Some(Slot { x: 0, y, w, h })
    }

    fn upload(&self, slot: Slot, data: &[u8]) {
        let bpp = self.format.block_copy_size(None).unwrap_or(1);
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x: slot.x, y: slot.y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(slot.w * bpp),
                rows_per_image: Some(slot.h),
            },
            wgpu::Extent3d { width: slot.w, height: slot.h, depth_or_array_layers: 1 },
        );
    }
}
