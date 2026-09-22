//! Small generational arena used by the transferred renderer's GPU resources.

use std::{marker::PhantomData, num::NonZeroU32};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct KeyData {
    pub index: u32,
    pub version: NonZeroU32,
}

pub trait SlotKey: Copy + Eq {
    fn new(index: u32, version: NonZeroU32) -> Self;
    fn data(self) -> KeyData;
}

macro_rules! define_slot_key {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub struct $name(KeyData);

        impl $name {
            pub fn new(index: u32, version: NonZeroU32) -> Self {
                Self(KeyData { index, version })
            }

            pub fn data(self) -> KeyData {
                self.0
            }
        }

        impl SlotKey for $name {
            fn new(index: u32, version: NonZeroU32) -> Self {
                Self::new(index, version)
            }

            fn data(self) -> KeyData {
                self.data()
            }
        }
    };
}

define_slot_key!(RendererMaterialHandle);
define_slot_key!(RendererMeshHandle);
define_slot_key!(RendererCameraHandle);
define_slot_key!(RendererTextureHandle);
define_slot_key!(RendererShaderHandle);

struct Slot<V> {
    version: NonZeroU32,
    value: Option<V>,
}

pub struct SlotMap<K, V> {
    slots: Vec<Slot<V>>,
    free: Vec<u32>,
    marker: PhantomData<fn() -> K>,
}

impl<K: SlotKey, V> SlotMap<K, V> {
    pub fn with_capacity_and_key(capacity: usize) -> Self {
        Self {
            slots: Vec::with_capacity(capacity),
            free: Vec::new(),
            marker: PhantomData,
        }
    }

    pub fn insert(&mut self, value: V) -> K {
        if let Some(index) = self.free.pop() {
            let slot = &mut self.slots[index as usize];
            slot.value = Some(value);
            return K::new(index, slot.version);
        }
        let index = self.slots.len() as u32;
        let version = NonZeroU32::new(1).unwrap();
        self.slots.push(Slot {
            version,
            value: Some(value),
        });
        K::new(index, version)
    }

    pub fn get(&self, key: K) -> Option<&V> {
        let data = key.data();
        let slot = self.slots.get(data.index as usize)?;
        (slot.version == data.version)
            .then_some(slot.value.as_ref())
            .flatten()
    }

    pub fn get_mut(&mut self, key: K) -> Option<&mut V> {
        let data = key.data();
        let slot = self.slots.get_mut(data.index as usize)?;
        (slot.version == data.version)
            .then_some(slot.value.as_mut())
            .flatten()
    }

    pub fn clear(&mut self) {
        for index in 0..self.slots.len() {
            if self.slots[index].value.take().is_some() {
                let next = self.slots[index].version.get().wrapping_add(1).max(1);
                self.slots[index].version = NonZeroU32::new(next).unwrap();
                self.free.push(index as u32);
            }
        }
    }
}
