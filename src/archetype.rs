// Copyright 2019 Google LLC
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

use crate::alloc::boxed::Box;
use crate::alloc::{vec, vec::Vec};
use crate::{
    alloc::alloc::{alloc, dealloc, Layout},
    dynamic_query::DynamicQueryTypes,
};
use core::any::{type_name, TypeId};
use core::cell::UnsafeCell;
use core::hash::{BuildHasher, BuildHasherDefault, Hasher};
use core::ops::Deref;
use core::ptr::{self, NonNull};
use core::{fmt, mem, slice};

use hashbrown::{hash_map::DefaultHashBuilder, HashMap};

use crate::borrow::AtomicBorrow;
use crate::query::Fetch;
use crate::{align, Access, Component, Query};

/// A collection of entities having the same component types
///
/// Accessing `Archetype`s is only required for complex dynamic scheduling. To manipulate entities,
/// go through the `World`.
pub struct Archetype {
    types: Vec<TypeInfo>,
    state: TypeIdMap<TypeState>,
    len: u32,
    entities: Box<[u32]>,
    // UnsafeCell allows unique references into `data` to be constructed while shared references
    // containing the `Archetype` exist
    data: UnsafeCell<NonNull<u8>>,
    data_size: usize,
}

impl Archetype {
    fn assert_type_info(types: &[TypeInfo]) {
        types.windows(2).for_each(|x| match x[0].cmp(&x[1]) {
            core::cmp::Ordering::Less => (),
            #[cfg(debug_assertions)]
            core::cmp::Ordering::Equal => panic!(
                "attempted to allocate entity with duplicate {} components; \
                 each type must occur at most once!",
                x[0].type_name
            ),
            #[cfg(not(debug_assertions))]
            core::cmp::Ordering::Equal => panic!(
                "attempted to allocate entity with duplicate components; \
                 each type must occur at most once!"
            ),
            core::cmp::Ordering::Greater => panic!("type info is unsorted"),
        });
    }

    pub(crate) fn new(types: Vec<TypeInfo>) -> Self {
        Self::assert_type_info(&types);
        Self {
            types,
            state: HashMap::default(),
            entities: Box::new([]),
            len: 0,
            data: UnsafeCell::new(NonNull::dangling()),
            data_size: 0,
        }
    }

    pub(crate) fn clear(&mut self) {
        for ty in &self.types {
            for index in 0..self.len {
                unsafe {
                    let removed = self
                        .get_dynamic(ty.id, ty.layout.size(), index)
                        .unwrap()
                        .as_ptr();
                    (ty.drop)(removed);
                }
            }
        }
        self.len = 0;
    }

    /// Whether this archetype contains `T` components
    pub fn has<T: Component>(&self) -> bool {
        self.has_dynamic(TypeId::of::<T>())
    }

    /// Whether this archetype contains components with the type identified by `id`
    pub fn has_dynamic(&self, id: TypeId) -> bool {
        self.state.contains_key(&id)
    }

    pub(crate) fn get_base<T: Component>(&self) -> Option<NonNull<T>> {
        let state = self.state.get(&TypeId::of::<T>())?;
        Some(unsafe {
            NonNull::new_unchecked(
                (*self.data.get()).as_ptr().add(state.offset).cast::<T>() as *mut T
            )
        })
    }

    /// Get the `T` components of these entities, if present
    ///
    /// Useful for efficient serialization.
    pub fn get<T: Component>(&self) -> Option<ColumnRef<'_, T>> {
        let ptr = self.get_base::<T>()?;
        let column = unsafe { slice::from_raw_parts_mut(ptr.as_ptr(), self.len as usize) };
        self.borrow::<T>();
        Some(ColumnRef {
            archetype: self,
            column,
        })
    }

    pub(crate) fn borrow<T: Component>(&self) {
        if self
            .state
            .get(&TypeId::of::<T>())
            .map_or(false, |x| !x.borrow.borrow())
        {
            panic!("{} already borrowed uniquely", type_name::<T>());
        }
    }

    pub(crate) fn borrow_mut<T: Component>(&self) {
        if self
            .state
            .get(&TypeId::of::<T>())
            .map_or(false, |x| !x.borrow.borrow_mut())
        {
            panic!("{} already borrowed", type_name::<T>());
        }
    }

    pub(crate) fn release<T: Component>(&self) {
        if let Some(x) = self.state.get(&TypeId::of::<T>()) {
            x.borrow.release();
        }
    }

    pub(crate) fn release_mut<T: Component>(&self) {
        if let Some(x) = self.state.get(&TypeId::of::<T>()) {
            x.borrow.release_mut();
        }
    }

    /// Number of entities in this archetype
    #[inline]
    pub fn len(&self) -> u32 {
        self.len
    }

    /// Whether this archetype contains no entities
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub(crate) fn entities(&self) -> NonNull<u32> {
        unsafe { NonNull::new_unchecked(self.entities.as_ptr() as *mut _) }
    }

    pub(crate) fn entity_slice(&self) -> &[u32] {
        &self.entities[..self.len() as usize]
    }

    pub(crate) fn entity_id(&self, index: u32) -> u32 {
        self.entities[index as usize]
    }

    #[inline]
    pub(crate) fn set_entity_id(&mut self, index: usize, id: u32) {
        self.entities[index] = id;
    }

    pub(crate) fn types(&self) -> &[TypeInfo] {
        &self.types
    }

    /// Enumerate the types of the components of entities stored in this archetype.
    ///
    /// Convenient for dispatching logic which needs to be performed on sets of type ids.  For
    /// example, suppose you're building a scripting system, and you want to integrate the scripting
    /// language with your ECS. This functionality allows you to iterate through all of the
    /// archetypes of the world with [`World::archetypes()`](crate::World::archetypes()) and extract
    /// all possible combinations of component types which are currently stored in the `World`.
    /// From there, you can then create a mapping of archetypes to wrapper objects for your
    /// scripting language that provide functionality based off of the components of any given
    /// `Entity`, and bind them onto an `Entity` when passed into your scripting language by looking
    /// up the `Entity`'s archetype using `EntityRef::component_types`.
    pub fn component_types(&self) -> impl ExactSizeIterator<Item = TypeId> + '_ {
        self.types.iter().map(|typeinfo| typeinfo.id)
    }

    /// `index` must be in-bounds or just past the end
    pub(crate) unsafe fn get_dynamic(
        &self,
        ty: TypeId,
        size: usize,
        index: u32,
    ) -> Option<NonNull<u8>> {
        debug_assert!(index <= self.len);
        Some(NonNull::new_unchecked(
            (*self.data.get())
                .as_ptr()
                .add(self.state.get(&ty)?.offset + size * index as usize)
                .cast::<u8>(),
        ))
    }

    /// Every type must be written immediately after this call
    pub(crate) unsafe fn allocate(&mut self, id: u32) -> u32 {
        if self.len as usize == self.entities.len() {
            self.grow(self.len.max(64));
        }

        self.entities[self.len as usize] = id;
        self.len += 1;
        self.len - 1
    }

    pub(crate) unsafe fn set_len(&mut self, len: u32) {
        debug_assert!(len <= self.capacity());
        self.len = len;
    }

    pub(crate) fn reserve(&mut self, additional: u32) {
        if additional > (self.capacity() - self.len()) {
            self.grow(additional - (self.capacity() - self.len()));
        }
    }

    pub(crate) fn capacity(&self) -> u32 {
        self.entities.len() as u32
    }

    fn grow(&mut self, increment: u32) {
        let old_count = self.len as usize;
        let new_cap = self.entities.len() + increment as usize;
        let mut new_entities = vec![!0; new_cap].into_boxed_slice();
        new_entities[0..old_count].copy_from_slice(&self.entities[0..old_count]);
        self.entities = new_entities;

        let old_data_size = mem::replace(&mut self.data_size, 0);
        let mut state = HashMap::with_capacity_and_hasher(self.types.len(), Default::default());
        for ty in &self.types {
            self.data_size = align(self.data_size, ty.layout.align());
            state.insert(ty.id, TypeState::new(self.data_size));
            self.data_size += ty.layout.size() * new_cap;
        }
        let new_data = if self.data_size == 0 {
            NonNull::dangling()
        } else {
            unsafe {
                NonNull::new(alloc(
                    Layout::from_size_align(
                        self.data_size,
                        self.types.first().map_or(1, |x| x.layout.align()),
                    )
                    .unwrap(),
                ))
                .unwrap()
            }
        };
        if old_data_size != 0 {
            for ty in &self.types {
                let old_off = self.state.get(&ty.id).unwrap().offset;
                let new_off = state.get(&ty.id).unwrap().offset;
                unsafe {
                    ptr::copy_nonoverlapping(
                        (*self.data.get()).as_ptr().add(old_off),
                        new_data.as_ptr().add(new_off),
                        ty.layout.size() * old_count,
                    );
                }
            }
            unsafe {
                dealloc(
                    (*self.data.get()).as_ptr().cast(),
                    Layout::from_size_align_unchecked(
                        old_data_size,
                        self.types.first().map_or(1, |x| x.layout.align()),
                    ),
                );
            }
        }

        self.data = UnsafeCell::new(new_data);
        self.state = state;
    }

    /// Returns the ID of the entity moved into `index`, if any
    pub(crate) unsafe fn remove(&mut self, index: u32) -> Option<u32> {
        let last = self.len - 1;
        for ty in &self.types {
            let removed = self
                .get_dynamic(ty.id, ty.layout.size(), index)
                .unwrap()
                .as_ptr();
            (ty.drop)(removed);
            if index != last {
                ptr::copy_nonoverlapping(
                    self.get_dynamic(ty.id, ty.layout.size(), last)
                        .unwrap()
                        .as_ptr(),
                    removed,
                    ty.layout.size(),
                );
            }
        }
        self.len = last;
        if index != last {
            self.entities[index as usize] = self.entities[last as usize];
            Some(self.entities[last as usize])
        } else {
            None
        }
    }

    /// Returns the ID of the entity moved into `index`, if any
    pub(crate) unsafe fn move_to(
        &mut self,
        index: u32,
        mut f: impl FnMut(*mut u8, TypeId, usize),
    ) -> Option<u32> {
        let last = self.len - 1;
        for ty in &self.types {
            let moved = self
                .get_dynamic(ty.id, ty.layout.size(), index)
                .unwrap()
                .as_ptr();
            f(moved, ty.id(), ty.layout().size());
            if index != last {
                ptr::copy_nonoverlapping(
                    self.get_dynamic(ty.id, ty.layout.size(), last)
                        .unwrap()
                        .as_ptr(),
                    moved,
                    ty.layout.size(),
                );
            }
        }
        self.len -= 1;
        if index != last {
            self.entities[index as usize] = self.entities[last as usize];
            Some(self.entities[last as usize])
        } else {
            None
        }
    }

    pub(crate) unsafe fn put_dynamic(
        &mut self,
        component: *mut u8,
        ty: TypeId,
        size: usize,
        index: u32,
    ) {
        let ptr = self
            .get_dynamic(ty, size, index)
            .unwrap()
            .as_ptr()
            .cast::<u8>();
        ptr::copy_nonoverlapping(component, ptr, size);
    }

    /// How, if at all, `Q` will access entities in this archetype
    pub fn access<Q: Query>(&self) -> Option<Access> {
        Q::Fetch::access(self)
    }

    pub(crate) fn access_dynamic(&self, query: &DynamicQueryTypes) -> Option<Access> {
        let mut access = None;
        for &read_component in query.read_types {
            if self.has_dynamic(read_component) {
                access = access.max(Some(Access::Read));
            } else {
                return None;
            }
        }
        for &write_component in query.write_types {
            if self.has_dynamic(write_component) {
                access = access.max(Some(Access::Write));
            } else {
                return None;
            }
        }
        access
    }

    pub(crate) fn component_layout(&self, component_type: TypeId) -> Option<Layout> {
        self.types()
            .iter()
            .find(|typ| typ.id == component_type)
            .map(|info| info.layout)
    }

    /// Add components from another archetype with identical components
    ///
    /// # Safety
    ///
    /// Component types must match exactly.
    pub(crate) unsafe fn merge(&mut self, mut other: Archetype) {
        self.reserve(other.len);
        for info in &self.types {
            let src_off = other.state.get(&info.id()).unwrap().offset;
            let src = (*other.data.get()).as_ptr().add(src_off);
            let dst_off = self.state.get(&info.id()).unwrap().offset;
            let dst = (*self.data.get())
                .as_ptr()
                .add(dst_off + self.len as usize * info.layout.size());
            dst.copy_from_nonoverlapping(src, other.len as usize * info.layout.size())
        }
        self.len += other.len;
        other.len = 0;
    }

    /// Raw IDs of the entities in this archetype
    ///
    /// Convertible into [`Entity`](crate::Entity)s with
    /// [`World::find_entity_from_id()`](crate::World::find_entity_from_id). Useful for efficient
    /// serialization.
    #[inline]
    pub fn ids(&self) -> &[u32] {
        &self.entities[0..self.len as usize]
    }
}

impl Drop for Archetype {
    fn drop(&mut self) {
        self.clear();
        if self.data_size != 0 {
            unsafe {
                dealloc(
                    (*self.data.get()).as_ptr().cast(),
                    Layout::from_size_align_unchecked(
                        self.data_size,
                        self.types.first().map_or(1, |x| x.layout.align()),
                    ),
                );
            }
        }
    }
}

/// A hasher optimized for hashing a single TypeId.
///
/// TypeId is already thoroughly hashed, so there's no reason to hash it again.
/// Just leave the bits unchanged.
#[derive(Default)]
pub(crate) struct TypeIdHasher {
    hash: u64,
}

impl Hasher for TypeIdHasher {
    fn write_u64(&mut self, n: u64) {
        // Only a single value can be hashed, so the old hash should be zero.
        debug_assert_eq!(self.hash, 0);
        self.hash = n;
    }

    // Tolerate TypeId being either u64 or u128.
    fn write_u128(&mut self, n: u128) {
        debug_assert_eq!(self.hash, 0);
        self.hash = n as u64;
    }

    fn write(&mut self, bytes: &[u8]) {
        debug_assert_eq!(self.hash, 0);

        // This will only be called if TypeId is neither u64 nor u128, which is not anticipated.
        // In that case we'll just fall back to using a different hash implementation.
        let mut hasher = <DefaultHashBuilder as BuildHasher>::Hasher::default();
        hasher.write(bytes);
        self.hash = hasher.finish();
    }

    fn finish(&self) -> u64 {
        self.hash
    }
}

/// A HashMap with TypeId keys
///
/// Because TypeId is already a fully-hashed u64 (including data in the high seven bits,
/// which hashbrown needs), there is no need to hash it again. Instead, this uses the much
/// faster no-op hash.
pub(crate) type TypeIdMap<V> = HashMap<TypeId, V, BuildHasherDefault<TypeIdHasher>>;

struct TypeState {
    offset: usize,
    borrow: AtomicBorrow,
}

impl TypeState {
    fn new(offset: usize) -> Self {
        Self {
            offset,
            borrow: AtomicBorrow::new(),
        }
    }
}

/// Metadata required to store a component
#[derive(Debug, Copy, Clone)]
pub struct TypeInfo {
    id: TypeId,
    layout: Layout,
    drop: unsafe fn(*mut u8),
    #[cfg(debug_assertions)]
    type_name: &'static str,
}

impl TypeInfo {
    /// Metadata for `T`
    pub fn of<T: 'static>() -> Self {
        unsafe fn drop_ptr<T>(x: *mut u8) {
            x.cast::<T>().drop_in_place()
        }

        Self {
            id: TypeId::of::<T>(),
            layout: Layout::new::<T>(),
            drop: drop_ptr::<T>,
            #[cfg(debug_assertions)]
            type_name: core::any::type_name::<T>(),
        }
    }

    pub(crate) fn id(&self) -> TypeId {
        self.id
    }

    pub(crate) fn layout(&self) -> Layout {
        self.layout
    }

    pub(crate) unsafe fn drop(&self, data: *mut u8) {
        (self.drop)(data)
    }
}

impl PartialOrd for TypeInfo {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TypeInfo {
    /// Order by alignment, descending. Ties broken with TypeId.
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.layout
            .align()
            .cmp(&other.layout.align())
            .reverse()
            .then_with(|| self.id.cmp(&other.id))
    }
}

impl PartialEq for TypeInfo {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for TypeInfo {}

/// Shared reference to a single column of component data in an [`Archetype`]
pub struct ColumnRef<'a, T: Component> {
    archetype: &'a Archetype,
    column: &'a [T],
}

impl<T: Component> Deref for ColumnRef<'_, T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        self.column
    }
}

impl<T: Component> Drop for ColumnRef<'_, T> {
    fn drop(&mut self) {
        self.archetype.release::<T>();
    }
}

impl<T: Component> Clone for ColumnRef<'_, T> {
    fn clone(&self) -> Self {
        self.archetype.borrow::<T>();
        Self {
            archetype: self.archetype,
            column: self.column,
        }
    }
}

impl<T: Component + fmt::Debug> fmt::Debug for ColumnRef<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.column.fmt(f)
    }
}
