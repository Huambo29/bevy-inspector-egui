pub(crate) mod impls;
mod inspectable_registry;
mod plugin;

pub use impls::InspectorQuery;
pub use inspectable_registry::InspectableRegistry;
pub use plugin::WorldInspectorPlugin;

use bevy::{
    ecs::{
        component::{ComponentId, ComponentInfo, ComponentTicks, StorageType},
        entity::EntityLocation,
        query::{FilterFetch, WorldQuery},
    },
    prelude::*,
    reflect::{TypeRegistryArc, TypeRegistryInternal},
    render::render_graph::base::MainPass,
    utils::HashSet,
};
use bevy_egui::egui::{self, Color32};
use egui::CollapsingHeader;
use pretty_type_name::pretty_type_name_str;
use std::{any::TypeId, borrow::Cow, cell::Cell};

use crate::{utils::sort_iter_if, Context};
use impls::EntityAttributes;
use inspectable_registry::InspectCallback;

/// Resource which controls the way the world inspector is shown.
#[derive(Debug, Clone)]
pub struct WorldInspectorParams {
    /// these components will be ignored
    pub ignore_components: HashSet<TypeId>,
    /// these components will be read only
    pub read_only_components: HashSet<TypeId>,
    /// Whether to sort the components alphabetically
    pub sort_components: bool,
    /// Controls whether the world inspector is shown
    pub enabled: bool,
    /// Whether entities can be despawned
    pub despawnable_entities: bool,
}

impl WorldInspectorParams {
    fn empty() -> Self {
        WorldInspectorParams {
            ignore_components: HashSet::default(),
            read_only_components: HashSet::default(),
            sort_components: false,
            enabled: true,
            despawnable_entities: false,
        }
    }

    /// Add `T` to component ignore list
    pub fn ignore_component<T: 'static>(&mut self) {
        self.ignore_components.insert(TypeId::of::<T>());
    }

    fn should_ignore_component(&self, type_id: TypeId) -> bool {
        self.ignore_components.contains(&type_id)
    }

    fn is_read_only(&self, type_id: TypeId) -> bool {
        self.read_only_components.contains(&type_id)
    }

    fn entity_options(&self) -> EntityAttributes {
        EntityAttributes {
            despawnable: self.despawnable_entities,
        }
    }
}

impl Default for WorldInspectorParams {
    fn default() -> Self {
        let mut params = WorldInspectorParams::empty();

        params.ignore_components = [
            TypeId::of::<Name>(),
            TypeId::of::<Children>(),
            TypeId::of::<Parent>(),
            TypeId::of::<PreviousParent>(),
            TypeId::of::<MainPass>(),
            TypeId::of::<Draw>(),
            TypeId::of::<RenderPipelines>(),
        ]
        .iter()
        .copied()
        .collect();
        params.read_only_components = [TypeId::of::<GlobalTransform>()].iter().copied().collect();

        params
    }
}

struct WorldUIContext<'a> {
    world: &'a mut World,
    ui_ctx: &'a egui::CtxRef,
    delete_entity: Cell<Option<Entity>>,
}
impl<'a> WorldUIContext<'a> {
    fn new(ui_ctx: &'a egui::CtxRef, world: &'a mut World) -> WorldUIContext<'a> {
        WorldUIContext {
            world,
            ui_ctx,
            delete_entity: Cell::new(None),
        }
    }
}

impl Drop for WorldUIContext<'_> {
    fn drop(&mut self) {
        if let Some(entity) = self.delete_entity.get() {
            despawn_with_children_recursive(self.world, entity);
        }
    }
}

impl<'a> WorldUIContext<'a> {
    fn entity_name(&self, entity: Entity) -> Cow<'_, str> {
        match self.world.get::<Name>(entity) {
            Some(name) => name.as_str().into(),
            None => format!("Entity {}", entity.id()).into(),
        }
    }

    fn world_ui<F>(&mut self, ui: &mut egui::Ui, params: &WorldInspectorParams)
    where
        F: WorldQuery,
        F::Fetch: FilterFetch,
    {
        let mut root_entities = self.world.query_filtered::<Entity, (Without<Parent>, F)>();

        // the entities are unique themselves, because only one WorldInspector can exist
        let dummy_id = egui::Id::new(42);
        let entity_options = params.entity_options();

        for entity in root_entities.iter(self.world) {
            self.entity_ui(ui, entity, params, dummy_id, &entity_options);
        }
    }

    fn entity_ui(
        &self,
        ui: &mut egui::Ui,
        entity: Entity,
        params: &WorldInspectorParams,
        id: egui::Id,
        entity_options: &EntityAttributes,
    ) {
        CollapsingHeader::new(self.entity_name(entity))
            .id_source(id.with(entity))
            .show(ui, |ui| {
                self.entity_ui_inner(ui, entity, params, id, entity_options)
            });
    }

    fn entity_ui_inner(
        &self,
        ui: &mut egui::Ui,
        entity: Entity,
        params: &WorldInspectorParams,
        id: egui::Id,
        entity_options: &EntityAttributes,
    ) {
        let entity_ref = match self.world.get_entity(entity) {
            Some(entity_ref) => entity_ref,
            None => return drop(ui.label("Entity does not exist")),
        };
        let entity_location = entity_ref.location();
        let archetype = entity_ref.archetype();

        self.component_kind_ui(
            ui,
            archetype.table_components(),
            "Components",
            entity,
            entity_location,
            params,
            id,
        );
        self.component_kind_ui(
            ui,
            archetype.sparse_set_components(),
            "Components (Sparse)",
            entity,
            entity_location,
            params,
            id,
        );

        ui.separator();

        let children = self.world.get::<Children>(entity);
        if let Some(children) = children {
            ui.label("Children");
            for &child in children.iter() {
                self.entity_ui(ui, child, params, id, entity_options);
            }
        } else {
            ui.label("No children");
        }

        if entity_options.despawnable {
            if ui.colored_label(Color32::RED, "✖ Despawn").clicked() {
                self.delete_entity.set(Some(entity));
            }
        }
    }

    fn component_kind_ui(
        &self,
        ui: &mut egui::Ui,
        components: &[ComponentId],
        title: &str,
        entity: Entity,
        entity_location: EntityLocation,
        params: &WorldInspectorParams,
        id: egui::Id,
    ) {
        if !components.is_empty() {
            ui.label(title);

            let iter = components.iter().map(|component_id| {
                let component_info = self.world.components().get_info(*component_id).unwrap();
                let name = pretty_type_name_str(component_info.name());
                (name, component_info)
            });
            let iter = sort_iter_if(iter, params.sort_components, |a, b| a.0.cmp(&b.0));

            for (name, component_info) in iter {
                self.component_ui(
                    ui,
                    name,
                    entity,
                    entity_location,
                    component_info,
                    params,
                    id,
                );
            }
        }
    }

    fn component_ui(
        &self,
        ui: &mut egui::Ui,
        name: String,
        entity: Entity,
        entity_location: EntityLocation,
        component_info: &ComponentInfo,
        params: &WorldInspectorParams,
        id: egui::Id,
    ) {
        let type_id = match component_info.type_id() {
            Some(id) => id,
            None => {
                ui.label("No type id");
                return;
            }
        };

        if params.should_ignore_component(type_id) {
            return;
        }

        let inspectable_registry = self.world.get_resource::<InspectableRegistry>().unwrap();
        let type_registry = self.world.get_resource::<TypeRegistryArc>().unwrap();
        let type_registry = &*type_registry.internal.read();

        CollapsingHeader::new(name)
            .id_source(id.with(component_info.id()))
            .show(ui, |ui| {
                if params.is_read_only(type_id) {
                    ui.set_enabled(false);
                }

                let world_ptr = self.world as *const _ as *mut _;
                let context = unsafe {
                    Context::new_ptr(self.ui_ctx, world_ptr)
                        .with_id(component_info.id().index() as u64)
                };

                let could_display = unsafe {
                    try_display(
                        &self.world,
                        entity,
                        entity_location,
                        component_info,
                        type_id,
                        inspectable_registry,
                        type_registry,
                        ui,
                        &context,
                    )
                };

                if !could_display {
                    ui.label("Inspectable has not been defined for this component");
                }
            });
    }
}

/// Safety:
/// The `location` must point to a valid archetype and index,
/// and the function must have unique access to the components.
pub(crate) unsafe fn try_display(
    world: &World,
    entity: Entity,
    location: EntityLocation,
    component_info: &ComponentInfo,
    type_id: TypeId,
    inspectable_registry: &InspectableRegistry,
    type_registry: &TypeRegistryInternal,
    ui: &mut egui::Ui,
    context: &Context,
) -> bool {
    if let Some(inspect_callback) = inspectable_registry.impls.get(&type_id) {
        display_by_inspectable_registry(
            inspect_callback,
            world,
            location,
            entity,
            component_info,
            ui,
            context,
        );
        return true;
    }

    if component_info.layout().size() == 0 {
        return true;
    }

    if display_by_reflection(type_registry, type_id, world, entity, ui, context).is_some() {
        return true;
    }

    false
}

unsafe fn display_by_inspectable_registry(
    inspect_callback: &InspectCallback,
    world: &World,
    location: EntityLocation,
    entity: Entity,
    component_info: &ComponentInfo,
    ui: &mut egui::Ui,
    context: &Context,
) {
    let (ptr, ticks) =
        get_component_and_ticks(world, component_info.id(), entity, location).unwrap();
    let ticks = { &mut *ticks };
    ticks.set_changed(world.read_change_tick());

    inspect_callback(ptr, ui, &context);
}

fn display_by_reflection(
    type_registry: &TypeRegistryInternal,
    type_id: TypeId,
    world: &World,
    entity: Entity,
    ui: &mut egui::Ui,
    context: &Context,
) -> Option<()> {
    let registration = type_registry.get(type_id)?;
    let reflect_component = registration.data::<ReflectComponent>()?;
    let mut reflected =
        unsafe { reflect_component.reflect_component_unchecked_mut(world, entity)? };
    crate::reflect::ui_for_reflect(&mut *reflected, ui, &context);
    Some(())
}

// copied from bevy
#[inline]
unsafe fn get_component_and_ticks(
    world: &World,
    component_id: ComponentId,
    entity: Entity,
    location: EntityLocation,
) -> Option<(*mut u8, *mut ComponentTicks)> {
    let archetype = &world.archetypes()[location.archetype_id];
    let component_info = world.components().get_info_unchecked(component_id);
    match component_info.storage_type() {
        StorageType::Table => {
            let table = &world.storages().tables[archetype.table_id()];
            let components = table.get_column(component_id)?;
            let table_row = archetype.entity_table_row(location.index);
            // SAFE: archetypes only store valid table_rows and the stored component type is T
            Some((
                components.get_unchecked(table_row),
                components.get_ticks_unchecked(table_row),
            ))
        }
        StorageType::SparseSet => world
            .storages()
            .sparse_sets
            .get(component_id)
            .and_then(|sparse_set| sparse_set.get_with_ticks(entity)),
    }
}
