// Copyright 2019 Google LLC
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

use crate::{alloc::vec::Vec, DynamicQuery, DynamicQueryTypes};
use core::any::TypeId;
use core::convert::TryFrom;
use core::{fmt, mem, ptr};

#[cfg(feature = "std")]
use std::error::Error;

use hashbrown::{HashMap, HashSet};

use crate::alloc::boxed::Box;
use crate::archetype::Archetype;
use crate::entities::{Entities, Location, ReserveEntitiesIterator};
use crate::{
    Bundle, ColumnBatch, DynamicBundle, Entity, EntityRef, Fetch, MissingComponent, NoSuchEntity,
    Query, QueryBorrow, QueryItem, QueryMut, QueryOne, Ref, RefMut,
};

/// An unordered collection of entities, each having any number of distinctly typed components
///
/// Similar to `HashMap<Entity, Vec<Box<dyn Any>>>` where each `Vec` never contains two of the same
/// type, but far more efficient to traverse.
///
/// The components of entities who have the same set of component types are stored in contiguous
/// runs, allowing for extremely fast, cache-friendly iteration.
///
/// There is a maximum number of unique entity IDs, which means that there is a maximum number of live
/// entities. When old entities are despawned, their IDs will be reused on a future entity, and
/// old `Entity` values with that ID will be invalidated.
///
/// ### Collisions
///
/// If an entity is despawned and its `Entity` handle is preserved over the course of billions of
/// following spawns and despawns, that handle may, in rare circumstances, collide with a
/// newly-allocated `Entity` handle. Very long-lived applications should therefore limit the period
/// over which they may retain handles of despawned entities.
pub struct World {
    entities: Entities,
    index: HashMap<Box<[TypeId]>, u32>,
    archetypes: Vec<Archetype>,
    archetype_generation: u64,
}

impl World {
    /// Create an empty world
    pub fn new() -> Self {
        // `flush` assumes archetype 0 always exists, representing entities with no components.
        let mut archetypes = Vec::new();
        archetypes.push(Archetype::new(Vec::new()));
        let mut index = HashMap::default();
        index.insert(Box::default(), 0);
        Self {
            entities: Entities::default(),
            index,
            archetypes,
            archetype_generation: 0,
        }
    }

    /// Create an entity with certain components
    ///
    /// Returns the ID of the newly created entity.
    ///
    /// Arguments can be tuples, structs annotated with `#[derive(Bundle)]`, or the result of
    /// calling `build` on an `EntityBuilder`, which is useful if the set of components isn't
    /// statically known. To spawn an entity with only one component, use a one-element tuple like
    /// `(x,)`.
    ///
    /// Any type that satisfies `Send + Sync + 'static` can be used as a component.
    ///
    /// # Example
    /// ```
    /// # use hecs::*;
    /// let mut world = World::new();
    /// let a = world.spawn((123, "abc"));
    /// let b = world.spawn((456, true));
    /// ```
    pub fn spawn(&mut self, components: impl DynamicBundle) -> Entity {
        // Ensure all entity allocations are accounted for so `self.entities` can realloc if
        // necessary
        self.flush();

        let entity = self.entities.alloc();

        self.spawn_inner(entity, components);

        entity
    }

    /// Create an entity with certain components and a specific `Entity` handle.
    ///
    /// See `spawn`.
    ///
    /// Despawns any existing entity with the same `Entity::id`.
    ///
    /// Useful for easy handle-preserving deserialization. Be cautious resurrecting old `Entity`
    /// handles in already-populated worlds as it vastly increases the likelihood of collisions.
    ///
    /// # Example
    /// ```
    /// # use hecs::*;
    /// let mut world = World::new();
    /// let a = world.spawn((123, "abc"));
    /// let b = world.spawn((456, true));
    /// world.despawn(a);
    /// assert!(!world.contains(a));
    /// // all previous Entity values pointing to 'a' will be live again, instead pointing to the new entity.
    /// world.spawn_at(a, (789, "ABC"));
    /// assert!(world.contains(a));
    /// ```
    pub fn spawn_at(&mut self, handle: Entity, components: impl DynamicBundle) {
        // Ensure all entity allocations are accounted for so `self.entities` can realloc if
        // necessary
        self.flush();

        let loc = self.entities.alloc_at(handle);
        if let Some(loc) = loc {
            if let Some(moved) =
                unsafe { self.archetypes[loc.archetype as usize].remove(loc.index) }
            {
                self.entities.meta[moved as usize].location.index = loc.index;
            }
        }

        self.spawn_inner(handle, components);
    }

    fn spawn_inner(&mut self, entity: Entity, components: impl DynamicBundle) {
        let archetype_id = components.with_ids(|ids| {
            self.index.get(ids).copied().unwrap_or_else(|| {
                let x = self.archetypes.len() as u32;
                self.archetypes.push(Archetype::new(components.type_info()));
                self.index.insert(ids.into(), x);
                self.archetype_generation += 1;
                x
            })
        });

        let archetype = &mut self.archetypes[archetype_id as usize];
        let index = unsafe { archetype.allocate(entity.id) };
        unsafe {
            components.put(|ptr, ty| {
                archetype.put_dynamic(ptr, ty.id(), ty.layout().size(), index);
            });
        }
        self.entities.meta[entity.id as usize].location = Location {
            archetype: archetype_id,
            index,
        };
    }

    /// Efficiently spawn a large number of entities with the same components
    ///
    /// Faster than calling `spawn` repeatedly with the same components.
    ///
    /// # Example
    /// ```
    /// # use hecs::*;
    /// let mut world = World::new();
    /// let entities = world.spawn_batch((0..1_000).map(|i| (i, "abc"))).collect::<Vec<_>>();
    /// for i in 0..1_000 {
    ///     assert_eq!(*world.get::<i32>(entities[i]).unwrap(), i as i32);
    /// }
    /// ```
    pub fn spawn_batch<I>(&mut self, iter: I) -> SpawnBatchIter<'_, I::IntoIter>
    where
        I: IntoIterator,
        I::Item: Bundle,
    {
        // Ensure all entity allocations are accounted for so `self.entities` can realloc if
        // necessary
        self.flush();

        let iter = iter.into_iter();
        let (lower, upper) = iter.size_hint();
        let archetype_id = self.reserve_inner::<I::Item>(
            u32::try_from(upper.unwrap_or(lower)).expect("iterator too large"),
        );

        SpawnBatchIter {
            inner: iter,
            entities: &mut self.entities,
            archetype_id,
            archetype: &mut self.archetypes[archetype_id as usize],
        }
    }

    /// Super-efficiently spawn the contents of a [`ColumnBatch`]
    ///
    /// The fastest, but most specialized, way to spawn large numbers of entities. Useful for high
    /// performance deserialization.
    pub fn spawn_column_batch(&mut self, batch: ColumnBatch) -> SpawnColumnBatchIter<'_> {
        self.flush();

        let archetype = batch.0;
        let entity_count = archetype.len();
        // Store component data
        let (archetype_id, base) = self.insert_archetype(archetype);

        let archetype = &mut self.archetypes[archetype_id as usize];
        let id_alloc = self.entities.alloc_many(entity_count, archetype_id, base);

        // Fix up entity IDs
        let mut id_alloc_clone = id_alloc.clone();
        let mut index = base as usize;
        while let Some(id) = id_alloc_clone.next(&self.entities) {
            archetype.set_entity_id(index, id);
            index += 1;
        }

        // Return iterator over new IDs
        SpawnColumnBatchIter {
            pending_end: id_alloc.pending_end,
            id_alloc,
            entities: &mut self.entities,
        }
    }

    /// Hybrid of `spawn_column_batch` and `spawn_at`
    pub fn spawn_column_batch_at(&mut self, handles: &[Entity], batch: ColumnBatch) {
        let archetype = batch.0;
        assert_eq!(
            handles.len(),
            archetype.len() as usize,
            "number of entity IDs {} must match number of entities {}",
            handles.len(),
            archetype.len()
        );

        // Drop components of entities that will be replaced
        for &handle in handles {
            let loc = self.entities.alloc_at(handle);
            if let Some(loc) = loc {
                if let Some(moved) =
                    unsafe { self.archetypes[loc.archetype as usize].remove(loc.index) }
                {
                    self.entities.meta[moved as usize].location.index = loc.index;
                }
            }
        }

        // Store components
        let (archetype_id, base) = self.insert_archetype(archetype);

        // Fix up entity IDs
        let archetype = &mut self.archetypes[archetype_id as usize];
        for (&handle, index) in handles.iter().zip(base as usize..) {
            archetype.set_entity_id(index, handle.id());
        }
    }

    /// Returns archetype ID and starting location index
    fn insert_archetype(&mut self, archetype: Archetype) -> (u32, u32) {
        use hashbrown::hash_map::Entry;

        let ids = archetype
            .types()
            .iter()
            .map(|info| info.id())
            .collect::<Box<_>>();

        match self.index.entry(ids) {
            Entry::Occupied(x) => {
                // Duplicate of existing archetype
                let existing = &mut self.archetypes[*x.get() as usize];
                let base = existing.len();
                unsafe {
                    existing.merge(archetype);
                }
                (*x.get(), base)
            }
            Entry::Vacant(x) => {
                // Brand new archetype
                let id = self.archetypes.len() as u32;
                self.archetypes.push(archetype);
                x.insert(id);
                (id, 0)
            }
        }
    }

    /// Allocate many entities ID concurrently
    ///
    /// Unlike `spawn`, this can be called simultaneously to other operations on the `World` such as
    /// queries, but does not immediately create the entities. Reserved entities are not visible to
    /// queries or world iteration, but can be otherwise operated on freely. Operations that
    /// uniquely borrow the world, such as `insert` or `despawn`, will cause all outstanding
    /// reserved entities to become real entities before proceeding. This can also be done
    /// explicitly by calling `flush`.
    ///
    /// Useful for reserving an ID that will later have components attached to it with `insert`.
    pub fn reserve_entities(&self, count: u32) -> ReserveEntitiesIterator {
        self.entities.reserve_entities(count)
    }

    /// Allocate an entity ID concurrently
    ///
    /// See `reserve_entities`.
    pub fn reserve_entity(&self) -> Entity {
        self.entities.reserve_entity()
    }

    /// Destroy an entity and all its components
    pub fn despawn(&mut self, entity: Entity) -> Result<(), NoSuchEntity> {
        self.flush();
        let loc = self.entities.free(entity)?;
        if let Some(moved) = unsafe { self.archetypes[loc.archetype as usize].remove(loc.index) } {
            self.entities.meta[moved as usize].location.index = loc.index;
        }
        Ok(())
    }

    /// Ensure `additional` entities with exact components `T` can be spawned without reallocating
    pub fn reserve<T: Bundle>(&mut self, additional: u32) {
        self.reserve_inner::<T>(additional);
    }

    fn reserve_inner<T: Bundle>(&mut self, additional: u32) -> u32 {
        self.flush();
        self.entities.reserve(additional);

        let archetype_id = T::with_static_ids(|ids| {
            self.index.get(ids).copied().unwrap_or_else(|| {
                let x = self.archetypes.len() as u32;
                self.archetypes.push(Archetype::new(T::static_type_info()));
                self.index.insert(ids.into(), x);
                self.archetype_generation += 1;
                x
            })
        });

        self.archetypes[archetype_id as usize].reserve(additional);
        archetype_id
    }

    /// Despawn all entities
    ///
    /// Preserves allocated storage for reuse.
    pub fn clear(&mut self) {
        for x in &mut self.archetypes {
            x.clear();
        }
        self.entities.clear();
    }

    /// Whether `entity` still exists
    pub fn contains(&self, entity: Entity) -> bool {
        self.entities.contains(entity)
    }

    /// Efficiently iterate over all entities that have certain components, using dynamic borrow
    /// checking
    ///
    /// Prefer `query_mut` when concurrent access to the `World` is not required.
    ///
    /// Calling `iter` on the returned value yields `(Entity, Q)` tuples, where `Q` is some query
    /// type. A query type is `&T`, `&mut T`, a tuple of query types, or an `Option` wrapping a
    /// query type, where `T` is any component type. Components queried with `&mut` must only appear
    /// once. Entities which do not have a component type referenced outside of an `Option` will be
    /// skipped.
    ///
    /// Entities are yielded in arbitrary order.
    ///
    /// The returned `QueryBorrow` can be further transformed with combinator methods; see its
    /// documentation for details.
    ///
    /// Iterating a query will panic if it would violate an existing unique reference or construct
    /// an invalid unique reference. This occurs when two simultaneously-active queries could expose
    /// the same entity. Simultaneous queries can access the same component type if and only if the
    /// world contains no entities that have all components required by both queries, assuming no
    /// other component borrows are outstanding.
    ///
    /// Iterating a query yields references with lifetimes bound to the `QueryBorrow` returned
    /// here. To ensure those are invalidated, the return value of this method must be dropped for
    /// its dynamic borrows from the world to be released. Similarly, lifetime rules ensure that
    /// references obtained from a query cannot outlive the `QueryBorrow`.
    ///
    /// # Example
    /// ```
    /// # use hecs::*;
    /// let mut world = World::new();
    /// let a = world.spawn((123, true, "abc"));
    /// let b = world.spawn((456, false));
    /// let c = world.spawn((42, "def"));
    /// let entities = world.query::<(&i32, &bool)>()
    ///     .iter()
    ///     .map(|(e, (&i, &b))| (e, i, b)) // Copy out of the world
    ///     .collect::<Vec<_>>();
    /// assert_eq!(entities.len(), 2);
    /// assert!(entities.contains(&(a, 123, true)));
    /// assert!(entities.contains(&(b, 456, false)));
    /// ```
    pub fn query<Q: Query>(&self) -> QueryBorrow<'_, Q> {
        QueryBorrow::new(&self.entities.meta, &self.archetypes)
    }

    /// Query a uniquely borrowed world
    ///
    /// Like `query`, but faster because dynamic borrow checks can be skipped. Note that, unlike
    /// `query`, this returns an `IntoIterator` which can be passed directly to a `for` loop.
    pub fn query_mut<Q: Query>(&mut self) -> QueryMut<'_, Q> {
        QueryMut::new(&self.entities.meta, &mut self.archetypes)
    }

    /// Perform a dynamic query.
    pub fn query_dynamic<'q>(&'q self, types: DynamicQueryTypes<'q>) -> DynamicQuery<'q> {
        DynamicQuery::new(types, &self.archetypes, &self.entities.meta)
    }

    /// Prepare a query against a single entity, using dynamic borrow checking
    ///
    /// Prefer `query_one_mut` when concurrent access to the `World` is not required.
    ///
    /// Call `get` on the resulting `QueryOne` to actually execute the query. The `QueryOne` value
    /// is responsible for releasing the dynamically-checked borrow made by `get`, so it can't be
    /// dropped while references returned by `get` are live.
    ///
    /// Handy for accessing multiple components simultaneously.
    ///
    /// # Example
    /// ```
    /// # use hecs::*;
    /// let mut world = World::new();
    /// let a = world.spawn((123, true, "abc"));
    /// // The returned query must outlive the borrow made by `get`
    /// let mut query = world.query_one::<(&mut i32, &bool)>(a).unwrap();
    /// let (number, flag) = query.get().unwrap();
    /// if *flag { *number *= 2; }
    /// assert_eq!(*number, 246);
    /// ```
    pub fn query_one<Q: Query>(&self, entity: Entity) -> Result<QueryOne<'_, Q>, NoSuchEntity> {
        let loc = self.entities.get(entity)?;
        Ok(unsafe { QueryOne::new(&self.archetypes[loc.archetype as usize], loc.index) })
    }

    /// Query a single entity in a uniquely borrow world
    ///
    /// Like `query_one`, but faster because dynamic borrow checks can be skipped. Note that, unlike
    /// `query_one`, on success this returns the query's results directly.
    pub fn query_one_mut<Q: Query>(
        &mut self,
        entity: Entity,
    ) -> Result<QueryItem<'_, Q>, QueryOneError> {
        let loc = self.entities.get(entity)?;
        unsafe {
            let fetch = Q::Fetch::new(&self.archetypes[loc.archetype as usize])
                .ok_or(QueryOneError::Unsatisfied)?;
            Ok(fetch.get(loc.index as usize))
        }
    }

    /// Borrow the `T` component of `entity`
    ///
    /// Panics if the component is already uniquely borrowed from another entity with the same
    /// components.
    pub fn get<T: Component>(&self, entity: Entity) -> Result<Ref<'_, T>, ComponentError> {
        let loc = self.entities.get(entity)?;
        if loc.archetype == 0 {
            return Err(MissingComponent::new::<T>().into());
        }
        Ok(unsafe { Ref::new(&self.archetypes[loc.archetype as usize], loc.index)? })
    }

    /// Uniquely borrow the `T` component of `entity`
    ///
    /// Panics if the component is already borrowed from another entity with the same components.
    pub fn get_mut<T: Component>(&self, entity: Entity) -> Result<RefMut<'_, T>, ComponentError> {
        let loc = self.entities.get(entity)?;
        if loc.archetype == 0 {
            return Err(MissingComponent::new::<T>().into());
        }
        Ok(unsafe { RefMut::new(&self.archetypes[loc.archetype as usize], loc.index)? })
    }

    /// Access an entity regardless of its component types
    ///
    /// Does not immediately borrow any component.
    pub fn entity(&self, entity: Entity) -> Result<EntityRef<'_>, NoSuchEntity> {
        Ok(match self.entities.get(entity)? {
            Location { archetype: 0, .. } => EntityRef::empty(),
            loc => unsafe { EntityRef::new(&self.archetypes[loc.archetype as usize], loc.index) },
        })
    }

    /// Given an id obtained from `Entity::id`, reconstruct the still-live `Entity`.
    ///
    /// # Safety
    /// `id` must correspond to a currently live `Entity`. A despawned or never-allocated `id` will produce undefined behavior.
    pub unsafe fn find_entity_from_id(&self, id: u32) -> Entity {
        self.entities.resolve_unknown_gen(id)
    }

    /// Iterate over all entities in the world
    ///
    /// Entities are yielded in arbitrary order. Prefer `World::query` for better performance when
    /// components will be accessed in predictable patterns.
    ///
    /// # Example
    /// ```
    /// # use hecs::*;
    /// let mut world = World::new();
    /// let a = world.spawn(());
    /// let b = world.spawn(());
    /// let ids = world.iter().map(|(id, _)| id).collect::<Vec<_>>();
    /// assert_eq!(ids.len(), 2);
    /// assert!(ids.contains(&a));
    /// assert!(ids.contains(&b));
    /// ```
    pub fn iter(&self) -> Iter<'_> {
        Iter::new(&self.archetypes, &self.entities)
    }

    /// Add `components` to `entity`
    ///
    /// Computational cost is proportional to the number of components `entity` has. If an entity
    /// already has a component of a certain type, it is dropped and replaced.
    ///
    /// When inserting a single component, see `insert_one` for convenience.
    ///
    /// # Example
    /// ```
    /// # use hecs::*;
    /// let mut world = World::new();
    /// let e = world.spawn((123, "abc"));
    /// world.insert(e, (456, true));
    /// assert_eq!(*world.get::<i32>(e).unwrap(), 456);
    /// assert_eq!(*world.get::<bool>(e).unwrap(), true);
    /// ```
    pub fn insert(
        &mut self,
        entity: Entity,
        components: impl DynamicBundle,
    ) -> Result<(), NoSuchEntity> {
        use hashbrown::hash_map::Entry;

        self.flush();
        let loc = self.entities.get_mut(entity)?;
            // Assemble Vec<TypeInfo> for the final entity
            let arch = &mut self.archetypes[loc.archetype as usize];
            let mut info = arch.types().to_vec();
            for ty in components.type_info() {
                if let Some(ptr) = unsafe { arch.get_dynamic(ty.id(), ty.layout().size(), loc.index) }{
                    unsafe { ty.drop(ptr.as_ptr()) };
                } else {
                    info.push(ty);
                }
            }
            info.sort();

            // Find the archetype it'll live in
            let elements = info.iter().map(|x| x.id()).collect();
            let target = match self.index.entry(elements) {
                Entry::Occupied(x) => *x.get(),
                Entry::Vacant(x) => {
                    let index = self.archetypes.len() as u32;
                    self.archetypes.push(Archetype::new(info));
                    x.insert(index);
                    self.archetype_generation += 1;
                    index
                }
            };

            if target == loc.archetype {
                // Update components in the current archetype
                let arch = &mut self.archetypes[loc.archetype as usize];
                unsafe {
                    components.put(|ptr, ty| {
                        arch.put_dynamic(ptr, ty.id(), ty.layout().size(), loc.index);
                    });
                }
                return Ok(());
            }

            // Move into a new archetype
            let (source_arch, target_arch) = index2(
                &mut self.archetypes,
                loc.archetype as usize,
                target as usize,
            );
            let target_index = unsafe { target_arch.allocate(entity.id) };
            loc.archetype = target;
            let old_index = mem::replace(&mut loc.index, target_index);
            if let Some(moved) = unsafe { source_arch.move_to(old_index, |ptr, ty, size| {
                target_arch.put_dynamic(ptr, ty, size, target_index);
            }) }{
                self.entities.meta[moved as usize].location.index = old_index;
            }
            unsafe {
                components.put(|ptr, ty| {
                    target_arch.put_dynamic(ptr, ty.id(), ty.layout().size(), target_index);
                });
            }
        Ok(())
    }

    /// Add `component` to `entity`
    ///
    /// See `insert`.
    pub fn insert_one(
        &mut self,
        entity: Entity,
        component: impl Component,
    ) -> Result<(), NoSuchEntity> {
        self.insert(entity, (component,))
    }

    /// Remove components from `entity`
    ///
    /// Computational cost is proportional to the number of components `entity` has. The entity
    /// itself is not removed, even if no components remain; use `despawn` for that. If any
    /// component in `T` is not present in `entity`, no components are removed and an error is
    /// returned.
    ///
    /// When removing a single component, see `remove_one` for convenience.
    ///
    /// # Example
    /// ```
    /// # use hecs::*;
    /// let mut world = World::new();
    /// let e = world.spawn((123, "abc", true));
    /// assert_eq!(world.remove::<(i32, &str)>(e), Ok((123, "abc")));
    /// assert!(world.get::<i32>(e).is_err());
    /// assert!(world.get::<&str>(e).is_err());
    /// assert_eq!(*world.get::<bool>(e).unwrap(), true);
    /// ```
    pub fn remove<T: Bundle>(&mut self, entity: Entity) -> Result<T, ComponentError> {
        use hashbrown::hash_map::Entry;

        self.flush();
        let loc = self.entities.get_mut(entity)?;
        unsafe {
            let removed = T::with_static_ids(|ids| ids.iter().copied().collect::<HashSet<_>>());
            let info = self.archetypes[loc.archetype as usize]
                .types()
                .iter()
                .cloned()
                .filter(|x| !removed.contains(&x.id()))
                .collect::<Vec<_>>();
            let elements = info.iter().map(|x| x.id()).collect();
            let target = match self.index.entry(elements) {
                Entry::Occupied(x) => *x.get(),
                Entry::Vacant(x) => {
                    self.archetypes.push(Archetype::new(info));
                    let index = (self.archetypes.len() - 1) as u32;
                    x.insert(index);
                    self.archetype_generation += 1;
                    index
                }
            };
            let old_index = loc.index;
            let source_arch = &self.archetypes[loc.archetype as usize];
            let bundle =
                T::get(|ty| source_arch.get_dynamic(ty.id(), ty.layout().size(), old_index))?;
            // If we actually removed any components, the entity needs to be moved into a new archetype
            if loc.archetype != target {
                let (source_arch, target_arch) = index2(
                    &mut self.archetypes,
                    loc.archetype as usize,
                    target as usize,
                );
                let target_index = target_arch.allocate(entity.id);
                loc.archetype = target;
                loc.index = target_index;
                if let Some(moved) = source_arch.move_to(old_index, |src, ty, size| {
                    // Only move the components present in the target archetype, i.e. the non-removed ones.
                    if let Some(dst) = target_arch.get_dynamic(ty, size, target_index) {
                        ptr::copy_nonoverlapping(src, dst.as_ptr(), size);
                    }
                }) {
                    self.entities.meta[moved as usize].location.index = old_index;
                }
            }
            Ok(bundle)
        }
    }

    /// Remove the `T` component from `entity`
    ///
    /// See `remove`.
    pub fn remove_one<T: Component>(&mut self, entity: Entity) -> Result<T, ComponentError> {
        self.remove::<(T,)>(entity).map(|(x,)| x)
    }

    /// Borrow the `T` component of `entity` without safety checks
    ///
    /// Should only be used as a building block for safe abstractions.
    ///
    /// # Safety
    ///
    /// `entity` must have been previously obtained from this `World`, and no unique borrow of the
    /// same component of `entity` may be live simultaneous to the returned reference.
    pub unsafe fn get_unchecked<T: Component>(&self, entity: Entity) -> Result<&T, ComponentError> {
        let loc = self.entities.get(entity)?;
        if loc.archetype == 0 {
            return Err(MissingComponent::new::<T>().into());
        }
        Ok(&*self.archetypes[loc.archetype as usize]
            .get_base::<T>()
            .ok_or_else(MissingComponent::new::<T>)?
            .as_ptr()
            .add(loc.index as usize))
    }

    /// Uniquely borrow the `T` component of `entity` without safety checks
    ///
    /// Should only be used as a building block for safe abstractions.
    ///
    /// # Safety
    ///
    /// `entity` must have been previously obtained from this `World`, and no borrow of the same
    /// component of `entity` may be live simultaneous to the returned reference.
    pub unsafe fn get_unchecked_mut<T: Component>(
        &self,
        entity: Entity,
    ) -> Result<&mut T, ComponentError> {
        let loc = self.entities.get(entity)?;
        if loc.archetype == 0 {
            return Err(MissingComponent::new::<T>().into());
        }
        Ok(&mut *self.archetypes[loc.archetype as usize]
            .get_base::<T>()
            .ok_or_else(MissingComponent::new::<T>)?
            .as_ptr()
            .add(loc.index as usize))
    }

    /// Convert all reserved entities into empty entities that can be iterated and accessed
    ///
    /// Invoked implicitly by `spawn`, `despawn`, `insert`, and `remove`.
    pub fn flush(&mut self) {
        let arch = &mut self.archetypes[0];
        self.entities
            .flush(|id, location| location.index = unsafe { arch.allocate(id) });
    }

    /// Inspect the archetypes that entities are organized into
    ///
    /// Useful for dynamically scheduling concurrent queries by checking borrows in advance, and for
    /// efficient serialization.
    pub fn archetypes(&self) -> impl ExactSizeIterator<Item = &'_ Archetype> + '_ {
        self.archetypes.iter()
    }

    /// Returns a distinct value after `archetypes` is changed
    ///
    /// Store the current value after deriving information from `archetypes`, then check whether the
    /// value returned by this function differs before attempting an operation that relies on its
    /// correctness. Useful for determining whether e.g. a concurrent query execution plan is still
    /// correct.
    ///
    /// The generation may be, but is not necessarily, changed as a result of adding or removing any
    /// entity or component.
    ///
    /// # Example
    /// ```
    /// # use hecs::*;
    /// let mut world = World::new();
    /// let initial_gen = world.archetypes_generation();
    /// world.spawn((123, "abc"));
    /// assert_ne!(initial_gen, world.archetypes_generation());
    /// ```
    pub fn archetypes_generation(&self) -> ArchetypesGeneration {
        ArchetypesGeneration(self.archetype_generation)
    }

    /// Number of currently live entities
    #[inline]
    pub fn len(&self) -> u32 {
        self.entities.len()
    }

    /// Whether no entities are live
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

unsafe impl Send for World {}
unsafe impl Sync for World {}

impl Default for World {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> IntoIterator for &'a World {
    type IntoIter = Iter<'a>;
    type Item = (Entity, EntityRef<'a>);
    fn into_iter(self) -> Iter<'a> {
        self.iter()
    }
}

fn index2<T>(x: &mut [T], i: usize, j: usize) -> (&mut T, &mut T) {
    assert!(i != j);
    assert!(i < x.len());
    assert!(j < x.len());
    let ptr = x.as_mut_ptr();
    unsafe { (&mut *ptr.add(i), &mut *ptr.add(j)) }
}

/// Errors that arise when accessing components
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub enum ComponentError {
    /// The entity was already despawned
    NoSuchEntity,
    /// The entity did not have a requested component
    MissingComponent(MissingComponent),
}

#[cfg(feature = "std")]
impl Error for ComponentError {}

impl fmt::Display for ComponentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use ComponentError::*;
        match *self {
            NoSuchEntity => f.write_str("no such entity"),
            MissingComponent(ref x) => x.fmt(f),
        }
    }
}

impl From<NoSuchEntity> for ComponentError {
    fn from(NoSuchEntity: NoSuchEntity) -> Self {
        ComponentError::NoSuchEntity
    }
}

impl From<MissingComponent> for ComponentError {
    fn from(x: MissingComponent) -> Self {
        ComponentError::MissingComponent(x)
    }
}

/// Errors that arise when querying a single entity
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub enum QueryOneError {
    /// The entity was already despawned
    NoSuchEntity,
    /// The entity exists but does not satisfy the query
    Unsatisfied,
}

#[cfg(feature = "std")]
impl Error for QueryOneError {}

impl fmt::Display for QueryOneError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use QueryOneError::*;
        match *self {
            NoSuchEntity => f.write_str("no such entity"),
            Unsatisfied => f.write_str("unsatisfied"),
        }
    }
}

impl From<NoSuchEntity> for QueryOneError {
    fn from(NoSuchEntity: NoSuchEntity) -> Self {
        QueryOneError::NoSuchEntity
    }
}

/// Types that can be components, implemented automatically for all `Send + Sync + 'static` types
///
/// This is just a convenient shorthand for `Send + Sync + 'static`, and never needs to be
/// implemented manually.
pub trait Component: Send + Sync + 'static {}
impl<T: Send + Sync + 'static> Component for T {}

/// Iterator over all of a world's entities
pub struct Iter<'a> {
    archetypes: core::slice::Iter<'a, Archetype>,
    entities: &'a Entities,
    current: Option<&'a Archetype>,
    index: u32,
}

impl<'a> Iter<'a> {
    fn new(archetypes: &'a [Archetype], entities: &'a Entities) -> Self {
        Self {
            archetypes: archetypes.iter(),
            entities,
            current: None,
            index: 0,
        }
    }
}

unsafe impl Send for Iter<'_> {}
unsafe impl Sync for Iter<'_> {}

impl<'a> Iterator for Iter<'a> {
    type Item = (Entity, EntityRef<'a>);
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.current {
                None => {
                    self.current = Some(self.archetypes.next()?);
                    self.index = 0;
                }
                Some(current) => {
                    if self.index == current.len() as u32 {
                        self.current = None;
                        continue;
                    }
                    let index = self.index;
                    self.index += 1;
                    let id = current.entity_id(index);
                    return Some((
                        Entity {
                            id,
                            generation: self.entities.meta[id as usize].generation,
                        },
                        unsafe { EntityRef::new(current, index) },
                    ));
                }
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.len(), Some(self.len()))
    }
}

impl ExactSizeIterator for Iter<'_> {
    #[inline]
    fn len(&self) -> usize {
        self.entities.len() as usize
    }
}

impl<A: DynamicBundle> Extend<A> for World {
    fn extend<T>(&mut self, iter: T)
    where
        T: IntoIterator<Item = A>,
    {
        for x in iter {
            self.spawn(x);
        }
    }
}

impl<A: DynamicBundle> core::iter::FromIterator<A> for World {
    fn from_iter<I: IntoIterator<Item = A>>(iter: I) -> Self {
        let mut world = World::new();
        world.extend(iter);
        world
    }
}

/// Determines freshness of information derived from `World::archetypes`
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct ArchetypesGeneration(u64);

/// Entity IDs created by `World::spawn_batch`
pub struct SpawnBatchIter<'a, I>
where
    I: Iterator,
    I::Item: Bundle,
{
    inner: I,
    entities: &'a mut Entities,
    archetype_id: u32,
    archetype: &'a mut Archetype,
}

impl<I> Drop for SpawnBatchIter<'_, I>
where
    I: Iterator,
    I::Item: Bundle,
{
    fn drop(&mut self) {
        for _ in self {}
    }
}

impl<I> Iterator for SpawnBatchIter<'_, I>
where
    I: Iterator,
    I::Item: Bundle,
{
    type Item = Entity;

    fn next(&mut self) -> Option<Entity> {
        let components = self.inner.next()?;
        let entity = self.entities.alloc();
        unsafe {
            let index = self.archetype.allocate(entity.id);
            components.put(|ptr, ty| {
                self.archetype
                    .put_dynamic(ptr, ty.id(), ty.layout().size(), index);
            });
            self.entities.meta[entity.id as usize].location = Location {
                archetype: self.archetype_id,
                index,
            };
        }
        Some(entity)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<I, T> ExactSizeIterator for SpawnBatchIter<'_, I>
where
    I: ExactSizeIterator<Item = T>,
    T: Bundle,
{
    fn len(&self) -> usize {
        self.inner.len()
    }
}

/// Iterator over [`Entity`]s spawned by [`World::spawn_column_batch()`]
pub struct SpawnColumnBatchIter<'a> {
    pending_end: usize,
    id_alloc: crate::entities::AllocManyState,
    entities: &'a mut Entities,
}

impl Iterator for SpawnColumnBatchIter<'_> {
    type Item = Entity;

    fn next(&mut self) -> Option<Entity> {
        let id = self.id_alloc.next(&self.entities)?;
        Some(unsafe { self.entities.resolve_unknown_gen(id) })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.len(), Some(self.len()))
    }
}

impl ExactSizeIterator for SpawnColumnBatchIter<'_> {
    fn len(&self) -> usize {
        self.id_alloc.len(&self.entities)
    }
}

impl Drop for SpawnColumnBatchIter<'_> {
    fn drop(&mut self) {
        // Consume used freelist entries
        self.entities.finish_alloc_many(self.pending_end);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reuse_empty() {
        let mut world = World::new();
        let a = world.spawn(());
        world.despawn(a).unwrap();
        let b = world.spawn(());
        assert_eq!(a.id, b.id);
        assert_ne!(a.generation, b.generation);
    }

    #[test]
    fn spawn_at() {
        let mut world = World::new();
        let a = world.spawn(());
        world.despawn(a).unwrap();
        let b = world.spawn(());
        assert!(world.contains(b));
        assert_eq!(a.id, b.id);
        assert_ne!(a.generation, b.generation);
        world.spawn_at(a, ());
        assert!(!world.contains(b));
        assert_eq!(b.id, a.id);
        assert_ne!(b.generation, a.generation);
    }

    #[test]
    fn reuse_populated() {
        let mut world = World::new();
        let a = world.spawn((42,));
        assert_eq!(*world.get::<i32>(a).unwrap(), 42);
        world.despawn(a).unwrap();
        let b = world.spawn((true,));
        assert_eq!(a.id, b.id);
        assert_ne!(a.generation, b.generation);
        assert!(world.get::<i32>(b).is_err());
        assert!(*world.get::<bool>(b).unwrap());
    }

    #[test]
    fn remove_nothing() {
        let mut world = World::new();
        let a = world.spawn(("abc", 123));
        world.remove::<()>(a).unwrap();
    }
}
