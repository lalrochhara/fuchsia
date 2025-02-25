// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::model::{
        actions::{Action, ActionKey, ActionSet},
        component::{ComponentInstance, InstanceState, ResolvedInstanceState},
        error::ModelError,
    },
    async_trait::async_trait,
    cm_rust::{
        CapabilityDecl, CapabilityName, ComponentDecl, DependencyType, OfferDecl, OfferSource,
        OfferTarget, RegistrationSource, StorageDirectorySource, UseDecl, UseSource,
    },
    futures::future::select_all,
    maplit::hashset,
    moniker::{ChildMoniker, PartialChildMoniker},
    std::collections::{HashMap, HashSet},
    std::fmt,
    std::sync::Arc,
};

/// Shuts down all component instances in this component (stops them and guarantees they will never
/// be started again).
pub struct ShutdownAction {}

impl ShutdownAction {
    pub fn new() -> Self {
        Self {}
    }
}

#[async_trait]
impl Action for ShutdownAction {
    type Output = Result<(), ModelError>;
    async fn handle(&self, component: &Arc<ComponentInstance>) -> Self::Output {
        do_shutdown(component).await
    }
    fn key(&self) -> ActionKey {
        ActionKey::Shutdown
    }
}

/// A DependencyNode represents a provider or user of a capability. This
/// may be either a component or a component collection.
#[derive(Debug, PartialEq, Eq, Hash, Clone, PartialOrd, Ord)]
pub enum DependencyNode {
    Parent,
    Child(String),
    Collection(String),
}

/// Examines a group of StorageDecls looking for one whose name matches the
/// String passed in and whose source is a child. `None` is returned if either
/// no declaration has the specified name or the declaration represents an
/// offer from Self or Parent.
fn find_storage_provider(
    capabilities: &Vec<CapabilityDecl>,
    name: &CapabilityName,
) -> Option<String> {
    for decl in capabilities {
        match decl {
            CapabilityDecl::Storage(decl) if &decl.name == name => match &decl.source {
                StorageDirectorySource::Child(child) => {
                    return Some(child.to_string());
                }
                StorageDirectorySource::Self_ | StorageDirectorySource::Parent => {
                    return None;
                }
            },
            _ => {}
        }
    }
    None
}

async fn shutdown_component(target: ShutdownInfo) -> Result<ParentOrChildMoniker, ModelError> {
    match target.moniker {
        ParentOrChildMoniker::Parent => {
            // TODO: Put the parent in a "shutting down" state so that if it creates new instances
            // after this point, they are created in a shut down state.
            target.component.stop_instance(true).await?;
        }
        ParentOrChildMoniker::ChildMoniker(_) => {
            ActionSet::register(target.component, ShutdownAction::new()).await?;
        }
    }

    Ok(target.moniker.clone())
}

/// Structure which holds bidirectional capability maps used during the
/// shutdown process.
struct ShutdownJob {
    /// A map from users of capabilities to the components that provide those
    /// capabilities
    target_to_sources: HashMap<ParentOrChildMoniker, Vec<ParentOrChildMoniker>>,
    /// A map from providers of capabilities to those components which use the
    /// capabilities
    source_to_targets: HashMap<ParentOrChildMoniker, ShutdownInfo>,
}

/// ShutdownJob encapsulates the logic and state require to shutdown a component.
impl ShutdownJob {
    /// Creates a new ShutdownJob by examining the Component's declaration and
    /// runtime state to build up the necessary data structures to stop
    /// components in the component in dependency order.
    pub async fn new(
        instance: &Arc<ComponentInstance>,
        state: &ResolvedInstanceState,
    ) -> ShutdownJob {
        // `dependency_map` represents the dependency relationships between the nodes in this
        // realm (the children, and the parent), as expressed in the component's declaration.
        // This representation must be reconciled with the runtime state of the
        // component. This means mapping children in the declaration with the one
        // or more children that may exist in collections and one or more
        // instances with a matching PartialChildMoniker that may exist.
        // `dependency_map` maps server => clients (aka provider => consumers, or source => targets)
        let dependency_map = process_component_dependencies(state.decl());
        let mut source_to_targets: HashMap<ParentOrChildMoniker, ShutdownInfo> = HashMap::new();

        for (source, targets) in dependency_map {
            let dependents = get_shutdown_monikers(&targets, state);

            let singleton_source = hashset![source];
            // The shutdown target may be a collection, if so this will expand
            // the collection out into a list of all its members, otherwise it
            // contains a single component.
            let matching_sources: Vec<_> =
                get_shutdown_monikers(&singleton_source, state).into_iter().collect();
            for source in matching_sources {
                let component = match &source {
                    ParentOrChildMoniker::Parent => instance.clone(),
                    ParentOrChildMoniker::ChildMoniker(moniker) => {
                        state.get_child(&moniker).expect("component not found in children").clone()
                    }
                };

                source_to_targets.insert(
                    source.clone(),
                    ShutdownInfo { moniker: source, dependents: dependents.clone(), component },
                );
            }
        }
        // `target_to_sources` is the inverse of `source_to_targets`, and maps a target to all of
        // its dependencies. This inverse mapping gives us a way to do quick lookups when updating
        // `source_to_targets` as we shutdown components in execute().
        let mut target_to_sources: HashMap<ParentOrChildMoniker, Vec<ParentOrChildMoniker>> =
            HashMap::new();
        for provider in source_to_targets.values() {
            // All listed siblings are ones that depend on this child
            // and all those siblings must stop before this one
            for consumer in &provider.dependents {
                // Make or update a map entry for the consumer that points to the
                // list of siblings that offer it capabilities
                target_to_sources
                    .entry(consumer.clone())
                    .or_insert(vec![])
                    .push(provider.moniker.clone());
            }
        }
        let new_job = ShutdownJob { source_to_targets, target_to_sources };
        return new_job;
    }

    /// Perform shutdown of the Component that was used to create this ShutdownJob A Component must
    /// wait to shut down until all its children are shut down.  The shutdown procedure looks at
    /// the children, if any, and determines the dependency relationships of the children.
    pub async fn execute(&mut self) -> Result<(), ModelError> {
        // Relationship maps are maintained to track dependencies. A map is
        // maintained both from a Component to its dependents and from a Component to
        // that Component's dependencies. With this dependency tracking, the
        // children of the Component can be shut down progressively in dependency
        // order.
        //
        // The progressive shutdown of Component is performed in this order:
        // Note: These steps continue until the shutdown process is no longer
        // asynchronously waiting for any shut downs to complete.
        //   * Identify the one or more Component that have no dependents
        //   * A shutdown action is set to the identified components. During the
        //     shut down process, the result of the process is received
        //     asynchronously.
        //   * After a Component is shut down, the Component are removed from the list
        //     of dependents of the Component on which they had a dependency.
        //   * The list of Component is checked again to see which Component have no
        //     remaining dependents.

        // Look for any children that have no dependents
        let mut stop_targets = vec![];

        for moniker in self.source_to_targets.keys().map(|key| key.clone()).collect::<Vec<_>>() {
            let no_dependents = {
                let info = self.source_to_targets.get(&moniker).expect("key disappeared from map");
                info.dependents.is_empty()
            };
            if no_dependents {
                stop_targets.push(
                    self.source_to_targets.remove(&moniker).expect("key disappeared from map"),
                );
            }
        }

        let mut futs = vec![];
        // Continue while we have new stop targets or unfinished futures
        while !stop_targets.is_empty() || !futs.is_empty() {
            for target in stop_targets.drain(..) {
                futs.push(Box::pin(shutdown_component(target)));
            }

            let (moniker, _, remaining) = select_all(futs).await;
            futs = remaining;

            let moniker = moniker?;

            // Look up the dependencies of the component that stopped
            match self.target_to_sources.remove(&moniker) {
                Some(sources) => {
                    for source in sources {
                        let ready_to_stop = {
                            if let Some(info) = self.source_to_targets.get_mut(&source) {
                                info.dependents.remove(&moniker);
                                // Have all of this components dependents stopped?
                                info.dependents.is_empty()
                            } else {
                                // The component that provided a capability to
                                // the stopped component doesn't exist or
                                // somehow already stopped. This is unexpected.
                                panic!(
                                    "The component '{:?}' appears to have stopped before its \
                                     dependency '{:?}'",
                                    moniker, source
                                );
                            }
                        };

                        // This components had zero remaining dependents
                        if ready_to_stop {
                            stop_targets.push(
                                self.source_to_targets
                                    .remove(&source)
                                    .expect("A key that was just available has disappeared."),
                            );
                        }
                    }
                }
                None => {
                    // Oh well, component didn't have any dependencies
                }
            }
        }

        // We should have stopped all children, if not probably there is a
        // dependency cycle
        if !self.source_to_targets.is_empty() {
            panic!(
                "Something failed, all children should have been removed! {:?}",
                self.source_to_targets
            );
        }
        Ok(())
    }
}

async fn do_shutdown(component: &Arc<ComponentInstance>) -> Result<(), ModelError> {
    {
        let state = component.lock_state().await;
        {
            let exec_state = component.lock_execution().await;
            if exec_state.is_shut_down() {
                return Ok(());
            }
        }
        match *state {
            InstanceState::Resolved(ref s) => {
                let mut shutdown_job = ShutdownJob::new(component, s).await;
                drop(state);
                Box::pin(shutdown_job.execute()).await?;
                return Ok(());
            }
            InstanceState::New | InstanceState::Discovered | InstanceState::Purged => {}
        }
    }
    // Control flow arrives here if the component isn't resolved.
    // TODO: Put this component in a "shutting down" state so that if it creates new instances
    // after this point, they are created in a shut down state.
    component.stop_instance(true).await?;

    Ok(())
}

#[derive(Debug, Eq, PartialEq, Hash, Clone)]
enum ParentOrChildMoniker {
    Parent,
    ChildMoniker(ChildMoniker),
}

/// Used to track information during the shutdown process. The dependents
/// are all the component which must stop before the component represented
/// by this struct.
struct ShutdownInfo {
    // TODO(jmatt) reduce visibility of fields
    /// The identifier for this component
    pub moniker: ParentOrChildMoniker,
    /// The components that this component offers capabilities to
    pub dependents: HashSet<ParentOrChildMoniker>,
    pub component: Arc<ComponentInstance>,
}

impl fmt::Debug for ShutdownInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "moniker: '{:?}'", self.moniker)
    }
}

/// Given a set of DependencyNodes, find all the matching ParentOrChildMonikers in the supplied
/// Component.
fn get_shutdown_monikers(
    nodes: &HashSet<DependencyNode>,
    component_state: &ResolvedInstanceState,
) -> HashSet<ParentOrChildMoniker> {
    let mut deps: HashSet<ParentOrChildMoniker> = HashSet::new();
    let all_children = component_state.all_children();

    for node in nodes {
        match node {
            DependencyNode::Child(name) => {
                let dep_moniker = PartialChildMoniker::new(name.to_string(), None);
                let matching_children = component_state.get_all_child_monikers(&dep_moniker);
                for m in matching_children {
                    deps.insert(ParentOrChildMoniker::ChildMoniker(m));
                }
            }
            DependencyNode::Collection(name) => {
                for moniker in all_children.keys() {
                    match moniker.collection() {
                        Some(m) => {
                            if m == name {
                                deps.insert(ParentOrChildMoniker::ChildMoniker(moniker.clone()));
                            }
                        }
                        None => {}
                    }
                }
            }
            DependencyNode::Parent => {
                deps.insert(ParentOrChildMoniker::Parent);
            }
        }
    }
    deps
}

/// Maps a dependency node (parent, child or collection) to the nodes that depend on it.
pub type DependencyMap = HashMap<DependencyNode, HashSet<DependencyNode>>;

/// For a given ComponentDecl, parse it, identify capability dependencies
/// between children and collections in the ComponentDecl. A map is returned
/// which maps from a child to a set of other children to which that child
/// provides capabilities. The siblings to which the child offers capabilities
/// must be shut down before that child. This function panics if there is a
/// capability routing where either the source or target is not present in this
/// ComponentDecl. Panics are not expected because ComponentDecls should be
/// validated before this function is called.
pub fn process_component_dependencies(decl: &ComponentDecl) -> DependencyMap {
    let mut dependency_map: DependencyMap = decl
        .children
        .iter()
        .map(|c| (DependencyNode::Child(c.name.clone()), HashSet::new()))
        .collect();
    dependency_map.extend(
        decl.collections
            .iter()
            .map(|c| (DependencyNode::Collection(c.name.clone()), HashSet::new())),
    );
    dependency_map.insert(DependencyNode::Parent, HashSet::new());

    get_dependencies_from_offers(decl, &mut dependency_map);
    get_dependencies_from_environments(decl, &mut dependency_map);
    get_dependencies_from_uses(decl, &mut dependency_map);
    dependency_map
}

/// Loops through all the use declarations to determine if parents depend on child capabilities, and vice-versa.
fn get_dependencies_from_uses(decl: &ComponentDecl, dependency_map: &mut DependencyMap) {
    // - By default, all children are dependents of the parent (`decl`)
    // - If parent (`decl`) uses from child, then child's dependent is parent instead.
    let add_to_dependency_map = |dependency_map: &mut DependencyMap,
                                 source: &UseSource,
                                 dependency_type: &DependencyType| {
        match source {
            UseSource::Child(name) => {
                match dependency_map.get_mut(&DependencyNode::Child(name.to_string())) {
                    Some(targets) => {
                        if dependency_type == &DependencyType::Strong {
                            targets.insert(DependencyNode::Parent);
                        }
                    }
                    _ => {
                        panic!("A dependency went off the map!");
                    }
                }
            }
            // capabilities which are not `use` from child are not relevant.
            _ => {}
        }
    };
    for use_ in &decl.uses {
        match use_ {
            UseDecl::Protocol(decl) => {
                add_to_dependency_map(dependency_map, &decl.source, &decl.dependency_type)
            }
            UseDecl::Service(decl) => {
                add_to_dependency_map(dependency_map, &decl.source, &decl.dependency_type)
            }
            UseDecl::Directory(decl) => {
                add_to_dependency_map(dependency_map, &decl.source, &decl.dependency_type)
            }
            // storage doesn't have a `use` source; storage is always assumed to be from the parent.
            UseDecl::Storage(_decl) => {}
            UseDecl::Event(decl) => {
                add_to_dependency_map(dependency_map, &decl.source, &decl.dependency_type)
            }
            // event streams specify a `use` source.
            UseDecl::EventStream(_decl) => {}
        }
    }
    // Next, find all children/collections who don't provide for the parent, and make them the parent's dependents.
    let mut parent_dependents = dependency_map
        .remove(&DependencyNode::Parent)
        .expect("Parent was not found in the dependency_map");
    for (source, targets) in dependency_map.iter() {
        if !targets.contains(&DependencyNode::Parent) {
            parent_dependents.insert(source.clone());
        }
    }
    dependency_map.insert(DependencyNode::Parent, parent_dependents);
}

/// Loops through all the offer declarations to determine which siblings
/// provide capabilities to other siblings.
fn get_dependencies_from_offers(decl: &ComponentDecl, dependency_map: &mut DependencyMap) {
    for dep in &decl.offers {
        // Identify the source and target of the offer. We only care about
        // dependencies where the provider of the dependency is another child,
        // otherwise the capability comes from the parent or component manager
        // itself in which case the relationship is not relevant for ordering
        // here.
        let source_target_pairs = match dep {
            OfferDecl::Protocol(svc_offer) => {
                match svc_offer.dependency_type {
                    DependencyType::Strong => {}
                    DependencyType::Weak | DependencyType::WeakForMigration => {
                        // weak dependencies are ignored by this algorithm, because weak
                        // dependencies can be broken arbitrarily.
                        continue;
                    }
                }
                match &svc_offer.source {
                    OfferSource::Child(source) => match &svc_offer.target {
                        OfferTarget::Child(target) => vec![(
                            DependencyNode::Child(source.clone()),
                            DependencyNode::Child(target.clone()),
                        )],
                        OfferTarget::Collection(target) => vec![(
                            DependencyNode::Child(source.clone()),
                            DependencyNode::Collection(target.clone()),
                        )],
                    },
                    _ => {
                        // Capabilities offered by the parent, routed in from the component, or
                        // provided by the framework (based on some other capability) are not
                        // relevant.
                        continue;
                    }
                }
            }
            OfferDecl::Service(svc_offer) => {
                match &svc_offer.source {
                    OfferSource::Child(source) => match &svc_offer.target {
                        OfferTarget::Child(target) => vec![(
                            DependencyNode::Child(source.clone()),
                            DependencyNode::Child(target.clone()),
                        )],
                        OfferTarget::Collection(target) => vec![(
                            DependencyNode::Child(source.clone()),
                            DependencyNode::Collection(target.clone()),
                        )],
                    },
                    _ => {
                        // Capabilities offered by the parent, routed in from the component, or
                        // provided by the framework (based on some other capability) are not
                        // relevant.
                        continue;
                    }
                }
            }
            OfferDecl::Directory(dir_offer) => {
                match dir_offer.dependency_type {
                    DependencyType::Strong => {}
                    DependencyType::Weak | DependencyType::WeakForMigration => {
                        // weak dependencies are ignored by this algorithm, because weak
                        // dependencies can be broken arbitrarily.
                        continue;
                    }
                }
                match &dir_offer.source {
                    OfferSource::Child(source) => match &dir_offer.target {
                        OfferTarget::Child(target) => vec![(
                            DependencyNode::Child(source.clone()),
                            DependencyNode::Child(target.clone()),
                        )],
                        OfferTarget::Collection(target) => vec![(
                            DependencyNode::Child(source.clone()),
                            DependencyNode::Collection(target.clone()),
                        )],
                    },
                    _ => {
                        // Capabilities offered by the parent or routed in from
                        // the component are not relevant.
                        continue;
                    }
                }
            }
            OfferDecl::Storage(s) => {
                match &s.source {
                    OfferSource::Self_ => {
                        match find_storage_provider(&decl.capabilities, &s.source_name) {
                            Some(storage_source) => match &s.target {
                                OfferTarget::Child(target) => vec![(
                                    DependencyNode::Child(storage_source.clone()),
                                    DependencyNode::Child(target.clone()),
                                )],
                                OfferTarget::Collection(target) => vec![(
                                    DependencyNode::Child(storage_source.clone()),
                                    DependencyNode::Collection(target.clone()),
                                )],
                            },
                            None => {
                                // The storage offer is not from a child, so it
                                // can be ignored.
                                continue;
                            }
                        }
                    }
                    _ => {
                        // Capabilities coming from the parent aren't tracked.
                        continue;
                    }
                }
            }
            OfferDecl::Runner(runner_offer) => {
                match &runner_offer.source {
                    OfferSource::Child(source) => match &runner_offer.target {
                        OfferTarget::Child(target) => vec![(
                            DependencyNode::Child(source.clone()),
                            DependencyNode::Child(target.clone()),
                        )],
                        OfferTarget::Collection(target) => vec![(
                            DependencyNode::Child(source.clone()),
                            DependencyNode::Collection(target.clone()),
                        )],
                    },
                    _ => {
                        // Capabilities coming from the parent aren't tracked.
                        continue;
                    }
                }
            }
            OfferDecl::Resolver(resolver_offer) => {
                match &resolver_offer.source {
                    OfferSource::Child(source) => match &resolver_offer.target {
                        OfferTarget::Child(target) => vec![(
                            DependencyNode::Child(source.clone()),
                            DependencyNode::Child(target.clone()),
                        )],
                        OfferTarget::Collection(target) => vec![(
                            DependencyNode::Child(source.clone()),
                            DependencyNode::Collection(target.clone()),
                        )],
                    },
                    _ => {
                        // Capabilities coming from the parent aren't tracked.
                        continue;
                    }
                }
            }
            OfferDecl::Event(_) => {
                // Events aren't tracked as dependencies for shutdown.
                continue;
            }
        };

        for (capability_provider, capability_target) in source_target_pairs {
            if !dependency_map.contains_key(&capability_target) {
                panic!(
                    "This capability routing seems invalid, the target \
                     does not exist in this component. Source: {:?} Target: {:?}",
                    capability_provider, capability_target,
                );
            }

            let sibling_deps = dependency_map.get_mut(&capability_provider).expect(&format!(
                "This capability routing seems invalid, the source \
                 does not exist in this component. Source: {:?} Target: {:?}",
                capability_provider, capability_target,
            ));
            sibling_deps.insert(capability_target);
        }
    }
}

/// Loops through all the child and collection declarations to determine what siblings provide
/// capabilities to other siblings through an environment.
fn get_dependencies_from_environments(decl: &ComponentDecl, dependency_map: &mut DependencyMap) {
    let mut env_source_children = HashMap::new();
    for env in &decl.environments {
        env_source_children.insert(&env.name, vec![]);
        for runner in &env.runners {
            if let RegistrationSource::Child(source_child) = &runner.source {
                env_source_children.get_mut(&env.name).unwrap().push(source_child);
            }
        }
    }

    for dest_child in &decl.children {
        if let Some(env_name) = dest_child.environment.as_ref() {
            for source_child in env_source_children.get(env_name).expect(&format!(
                "environment `{}` from child `{}` is not a valid environment",
                env_name, dest_child.name,
            )) {
                dependency_map
                    .entry(DependencyNode::Child((*source_child).clone()))
                    .or_insert(HashSet::new())
                    .insert(DependencyNode::Child(dest_child.name.clone()));
            }
        }
    }
    for dest_collection in &decl.collections {
        if let Some(env_name) = dest_collection.environment.as_ref() {
            for source_child in env_source_children.get(env_name).expect(&format!(
                "environment `{}` from collection `{}` is not a valid environment",
                env_name, dest_collection.name,
            )) {
                dependency_map
                    .entry(DependencyNode::Child((*source_child).clone()))
                    .or_insert(HashSet::new())
                    .insert(DependencyNode::Collection(dest_collection.name.clone()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::model::{
            actions::{
                test_utils::{is_executing, is_unresolved},
                StopAction,
            },
            binding::Binder,
            component::BindReason,
            hooks::{self, EventPayload, EventType, Hook, HooksRegistration},
            testing::{
                test_helpers::{
                    component_decl_with_test_runner, default_component_decl,
                    execution_is_shut_down, has_child, ActionsTest, ComponentInfo,
                },
                test_hook::Lifecycle,
            },
        },
        async_trait::async_trait,
        cm_rust::{
            CapabilityName, CapabilityPath, ChildDecl, DependencyType, ExposeDecl,
            ExposeProtocolDecl, ExposeSource, ExposeTarget, OfferDecl, OfferProtocolDecl,
            OfferResolverDecl, OfferSource, OfferTarget, ProtocolDecl, UseDecl, UseProtocolDecl,
            UseSource,
        },
        cm_rust_testing::{
            ChildDeclBuilder, CollectionDeclBuilder, ComponentDeclBuilder, EnvironmentDeclBuilder,
        },
        fidl_fuchsia_sys2 as fsys,
        moniker::{AbsoluteMoniker, PartialChildMoniker},
        std::collections::HashMap,
        std::{convert::TryFrom, sync::Weak},
        test_case::test_case,
    };

    // TODO(jmatt) Add tests for all capability types

    /// Validates that actual looks like expected and panics if they don't.
    /// `expected` must be sorted and so must the second member of each
    /// tuple in the vec.
    fn validate_results(
        expected: Vec<(DependencyNode, Vec<DependencyNode>)>,
        mut actual: HashMap<DependencyNode, HashSet<DependencyNode>>,
    ) {
        let mut actual_sorted: Vec<(DependencyNode, Vec<DependencyNode>)> = actual
            .drain()
            .map(|(k, v)| {
                let mut new_vec = Vec::new();
                new_vec.extend(v.into_iter());
                new_vec.sort_unstable();
                (k, new_vec)
            })
            .collect();
        actual_sorted.sort_unstable();
        assert_eq!(expected, actual_sorted);
    }

    #[test]
    fn test_service_from_parent() {
        let decl = ComponentDecl {
            offers: vec![OfferDecl::Protocol(OfferProtocolDecl {
                source: OfferSource::Self_,
                source_name: "serviceParent".into(),
                target_name: "serviceParent".into(),
                target: OfferTarget::Child("childA".to_string()),
                dependency_type: DependencyType::Strong,
            })],
            children: vec![ChildDecl {
                name: "childA".to_string(),
                url: "ignored:///child".to_string(),
                startup: fsys::StartupMode::Lazy,
                environment: None,
            }],
            ..default_component_decl()
        };

        let mut expected: Vec<(DependencyNode, Vec<DependencyNode>)> = Vec::new();
        expected.push((DependencyNode::Parent, vec![DependencyNode::Child("childA".to_string())]));
        expected.push((DependencyNode::Child("childA".to_string()), vec![]));
        validate_results(expected, process_component_dependencies(&decl));
    }

    #[test_case(DependencyType::Weak)]
    #[test_case(DependencyType::WeakForMigration)]
    fn test_weak_service_from_parent(weak_dep: DependencyType) {
        let decl = ComponentDecl {
            offers: vec![OfferDecl::Protocol(OfferProtocolDecl {
                source: OfferSource::Self_,
                source_name: "serviceParent".into(),
                target_name: "serviceParent".into(),
                target: OfferTarget::Child("childA".to_string()),
                dependency_type: weak_dep,
            })],
            children: vec![ChildDecl {
                name: "childA".to_string(),
                url: "ignored:///child".to_string(),
                startup: fsys::StartupMode::Lazy,
                environment: None,
            }],
            ..default_component_decl()
        };

        let mut expected: Vec<(DependencyNode, Vec<DependencyNode>)> = Vec::new();
        expected.push((DependencyNode::Parent, vec![DependencyNode::Child("childA".to_string())]));
        expected.push((DependencyNode::Child("childA".to_string()), vec![]));
        validate_results(expected, process_component_dependencies(&decl));
    }

    #[test]
    fn test_service_from_child() {
        let decl = ComponentDecl {
            exposes: vec![ExposeDecl::Protocol(ExposeProtocolDecl {
                target: ExposeTarget::Parent,
                source_name: "serviceFromChild".into(),
                target_name: "serviceFromChild".into(),
                source: ExposeSource::Child("childA".to_string()),
            })],
            children: vec![ChildDecl {
                name: "childA".to_string(),
                url: "ignored:///child".to_string(),
                startup: fsys::StartupMode::Lazy,
                environment: None,
            }],
            ..default_component_decl()
        };

        let mut expected: Vec<(DependencyNode, Vec<DependencyNode>)> = Vec::new();
        expected.push((DependencyNode::Parent, vec![DependencyNode::Child("childA".to_string())]));
        expected.push((DependencyNode::Child("childA".to_string()), vec![]));
        validate_results(expected, process_component_dependencies(&decl));
    }

    #[test]
    fn test_single_dependency() {
        let child_a = ChildDecl {
            name: "childA".to_string(),
            url: "ignored:///child".to_string(),
            startup: fsys::StartupMode::Lazy,
            environment: None,
        };
        let child_b = ChildDecl {
            name: "childB".to_string(),
            url: "ignored:///child".to_string(),
            startup: fsys::StartupMode::Lazy,
            environment: None,
        };
        let decl = ComponentDecl {
            offers: vec![
                OfferDecl::Protocol(OfferProtocolDecl {
                    source: OfferSource::Self_,
                    source_name: "serviceParent".into(),
                    target_name: "serviceParent".into(),
                    target: OfferTarget::Child("childA".to_string()),
                    dependency_type: DependencyType::Strong,
                }),
                OfferDecl::Protocol(OfferProtocolDecl {
                    source: OfferSource::Child("childB".to_string()),
                    source_name: "childBOffer".into(),
                    target_name: "serviceSibling".into(),
                    target: OfferTarget::Child("childA".to_string()),
                    dependency_type: DependencyType::Strong,
                }),
            ],
            children: vec![child_a.clone(), child_b.clone()],
            ..default_component_decl()
        };

        let mut expected: Vec<(DependencyNode, Vec<DependencyNode>)> = Vec::new();
        let mut v = vec![DependencyNode::Child(child_a.name.clone())];
        v.sort_unstable();
        expected.push((DependencyNode::Child(child_b.name.clone()), v));
        expected.push((DependencyNode::Child(child_a.name.clone()), vec![]));
        expected.push((
            DependencyNode::Parent,
            vec![
                DependencyNode::Child(child_a.name.clone()),
                DependencyNode::Child(child_b.name.clone()),
            ],
        ));
        expected.sort_unstable();

        validate_results(expected, process_component_dependencies(&decl));
    }

    #[test]
    fn test_environment_with_runner_from_parent() {
        let decl = ComponentDecl {
            environments: vec![EnvironmentDeclBuilder::new()
                .name("env")
                .add_runner(cm_rust::RunnerRegistration {
                    source: RegistrationSource::Parent,
                    source_name: "foo".into(),
                    target_name: "foo".into(),
                })
                .build()],
            children: vec![
                ChildDeclBuilder::new_lazy_child("childA").build(),
                ChildDeclBuilder::new_lazy_child("childB").environment("env").build(),
            ],
            ..default_component_decl()
        };

        let mut expected: Vec<(DependencyNode, Vec<DependencyNode>)> = Vec::new();
        expected.push((
            DependencyNode::Parent,
            vec![
                DependencyNode::Child("childA".to_string()),
                DependencyNode::Child("childB".to_string()),
            ],
        ));
        expected.push((DependencyNode::Child("childA".to_string()), vec![]));
        expected.push((DependencyNode::Child("childB".to_string()), vec![]));
        validate_results(expected, process_component_dependencies(&decl));
    }

    #[test]
    fn test_environment_with_runner_from_child() {
        let decl = ComponentDecl {
            environments: vec![EnvironmentDeclBuilder::new()
                .name("env")
                .add_runner(cm_rust::RunnerRegistration {
                    source: RegistrationSource::Child("childA".into()),
                    source_name: "foo".into(),
                    target_name: "foo".into(),
                })
                .build()],
            children: vec![
                ChildDeclBuilder::new_lazy_child("childA").build(),
                ChildDeclBuilder::new_lazy_child("childB").environment("env").build(),
            ],
            ..default_component_decl()
        };

        let mut expected: Vec<(DependencyNode, Vec<DependencyNode>)> = Vec::new();
        expected.push((
            DependencyNode::Parent,
            vec![
                DependencyNode::Child("childA".to_string()),
                DependencyNode::Child("childB".to_string()),
            ],
        ));
        expected.push((
            DependencyNode::Child("childA".to_string()),
            vec![DependencyNode::Child("childB".to_string())],
        ));
        expected.push((DependencyNode::Child("childB".to_string()), vec![]));
        validate_results(expected, process_component_dependencies(&decl));
    }

    #[test]
    fn test_environment_with_runner_from_child_to_collection() {
        let decl = ComponentDecl {
            environments: vec![EnvironmentDeclBuilder::new()
                .name("env")
                .add_runner(cm_rust::RunnerRegistration {
                    source: RegistrationSource::Child("childA".into()),
                    source_name: "foo".into(),
                    target_name: "foo".into(),
                })
                .build()],
            collections: vec![CollectionDeclBuilder::new().name("coll").environment("env").build()],
            ..default_component_decl()
        };

        let mut expected: Vec<(DependencyNode, Vec<DependencyNode>)> = Vec::new();
        expected.push((
            DependencyNode::Parent,
            vec![
                DependencyNode::Child("childA".to_string()),
                DependencyNode::Collection("coll".to_string()),
            ],
        ));
        expected.push((
            DependencyNode::Child("childA".to_string()),
            vec![DependencyNode::Collection("coll".to_string())],
        ));
        expected.push((DependencyNode::Collection("coll".to_string()), vec![]));
        validate_results(expected, process_component_dependencies(&decl));
    }

    #[test]
    fn test_chained_environments() {
        let decl = ComponentDecl {
            environments: vec![
                EnvironmentDeclBuilder::new()
                    .name("env")
                    .add_runner(cm_rust::RunnerRegistration {
                        source: RegistrationSource::Child("childA".into()),
                        source_name: "foo".into(),
                        target_name: "foo".into(),
                    })
                    .build(),
                EnvironmentDeclBuilder::new()
                    .name("env2")
                    .add_runner(cm_rust::RunnerRegistration {
                        source: RegistrationSource::Child("childB".into()),
                        source_name: "bar".into(),
                        target_name: "bar".into(),
                    })
                    .build(),
            ],
            children: vec![
                ChildDeclBuilder::new_lazy_child("childA").build(),
                ChildDeclBuilder::new_lazy_child("childB").environment("env").build(),
                ChildDeclBuilder::new_lazy_child("childC").environment("env2").build(),
            ],
            ..default_component_decl()
        };

        let mut expected: Vec<(DependencyNode, Vec<DependencyNode>)> = Vec::new();
        expected.push((
            DependencyNode::Parent,
            vec![
                DependencyNode::Child("childA".to_string()),
                DependencyNode::Child("childB".to_string()),
                DependencyNode::Child("childC".to_string()),
            ],
        ));
        expected.push((
            DependencyNode::Child("childA".to_string()),
            vec![DependencyNode::Child("childB".to_string())],
        ));
        expected.push((
            DependencyNode::Child("childB".to_string()),
            vec![DependencyNode::Child("childC".to_string())],
        ));
        expected.push((DependencyNode::Child("childC".to_string()), vec![]));
        validate_results(expected, process_component_dependencies(&decl));
    }

    #[test]
    fn test_environment_and_offer() {
        let decl = ComponentDecl {
            offers: vec![OfferDecl::Protocol(OfferProtocolDecl {
                source: OfferSource::Child("childB".to_string()),
                source_name: "childBOffer".into(),
                target_name: "serviceSibling".into(),
                target: OfferTarget::Child("childC".to_string()),
                dependency_type: DependencyType::Strong,
            })],
            environments: vec![EnvironmentDeclBuilder::new()
                .name("env")
                .add_runner(cm_rust::RunnerRegistration {
                    source: RegistrationSource::Child("childA".into()),
                    source_name: "foo".into(),
                    target_name: "foo".into(),
                })
                .build()],
            children: vec![
                ChildDeclBuilder::new_lazy_child("childA").build(),
                ChildDeclBuilder::new_lazy_child("childB").environment("env").build(),
                ChildDeclBuilder::new_lazy_child("childC").build(),
            ],
            ..default_component_decl()
        };

        let mut expected: Vec<(DependencyNode, Vec<DependencyNode>)> = Vec::new();
        expected.push((
            DependencyNode::Parent,
            vec![
                DependencyNode::Child("childA".to_string()),
                DependencyNode::Child("childB".to_string()),
                DependencyNode::Child("childC".to_string()),
            ],
        ));
        expected.push((
            DependencyNode::Child("childA".to_string()),
            vec![DependencyNode::Child("childB".to_string())],
        ));
        expected.push((
            DependencyNode::Child("childB".to_string()),
            vec![DependencyNode::Child("childC".to_string())],
        ));
        expected.push((DependencyNode::Child("childC".to_string()), vec![]));
        validate_results(expected, process_component_dependencies(&decl));
    }

    #[test_case(DependencyType::Weak)]
    #[test_case(DependencyType::WeakForMigration)]
    fn test_single_weak_dependency(weak_dep: DependencyType) {
        let child_a = ChildDecl {
            name: "childA".to_string(),
            url: "ignored:///child".to_string(),
            startup: fsys::StartupMode::Lazy,
            environment: None,
        };
        let child_b = ChildDecl {
            name: "childB".to_string(),
            url: "ignored:///child".to_string(),
            startup: fsys::StartupMode::Lazy,
            environment: None,
        };
        let decl = ComponentDecl {
            offers: vec![
                OfferDecl::Protocol(OfferProtocolDecl {
                    source: OfferSource::Self_,
                    source_name: "serviceParent".into(),
                    target_name: "serviceParent".into(),
                    target: OfferTarget::Child("childA".to_string()),
                    dependency_type: weak_dep.clone(),
                }),
                OfferDecl::Protocol(OfferProtocolDecl {
                    source: OfferSource::Child("childB".to_string()),
                    source_name: "childBOffer".into(),
                    target_name: "serviceSibling".into(),
                    target: OfferTarget::Child("childA".to_string()),
                    dependency_type: weak_dep.clone(),
                }),
            ],
            children: vec![child_a.clone(), child_b.clone()],
            ..default_component_decl()
        };

        let mut expected: Vec<(DependencyNode, Vec<DependencyNode>)> = Vec::new();
        expected.push((
            DependencyNode::Parent,
            vec![
                DependencyNode::Child(child_a.name.clone()),
                DependencyNode::Child(child_b.name.clone()),
            ],
        ));
        expected.push((DependencyNode::Child(child_b.name.clone()), vec![]));
        expected.push((DependencyNode::Child(child_a.name.clone()), vec![]));
        expected.sort_unstable();

        validate_results(expected, process_component_dependencies(&decl));
    }

    #[test]
    fn test_multiple_dependencies_same_source() {
        let child_a = ChildDecl {
            name: "childA".to_string(),
            url: "ignored:///child".to_string(),
            startup: fsys::StartupMode::Lazy,
            environment: None,
        };
        let child_b = ChildDecl {
            name: "childB".to_string(),
            url: "ignored:///child".to_string(),
            startup: fsys::StartupMode::Lazy,
            environment: None,
        };
        let decl = ComponentDecl {
            offers: vec![
                OfferDecl::Protocol(OfferProtocolDecl {
                    source: OfferSource::Self_,
                    source_name: "serviceParent".into(),
                    target_name: "serviceParent".into(),
                    target: OfferTarget::Child("childA".to_string()),
                    dependency_type: DependencyType::Strong,
                }),
                OfferDecl::Protocol(OfferProtocolDecl {
                    source: OfferSource::Child("childB".to_string()),
                    source_name: "childBOffer".into(),
                    target_name: "serviceSibling".into(),
                    target: OfferTarget::Child("childA".to_string()),
                    dependency_type: DependencyType::Strong,
                }),
                OfferDecl::Protocol(OfferProtocolDecl {
                    source: OfferSource::Child("childB".to_string()),
                    source_name: "childBOtherOffer".into(),
                    target_name: "serviceOtherSibling".into(),
                    target: OfferTarget::Child("childA".to_string()),
                    dependency_type: DependencyType::Strong,
                }),
            ],
            children: vec![child_a.clone(), child_b.clone()],
            ..default_component_decl()
        };

        let mut expected: Vec<(DependencyNode, Vec<DependencyNode>)> = Vec::new();
        let mut v = vec![DependencyNode::Child(child_a.name.clone())];
        v.sort_unstable();
        expected.push((DependencyNode::Child(child_b.name.clone()), v));
        expected.push((DependencyNode::Child(child_a.name.clone()), vec![]));
        expected.push((
            DependencyNode::Parent,
            vec![
                DependencyNode::Child(child_a.name.clone()),
                DependencyNode::Child(child_b.name.clone()),
            ],
        ));
        expected.sort_unstable();

        validate_results(expected, process_component_dependencies(&decl));
    }

    #[test]
    fn test_multiple_dependents_same_source() {
        let child_a = ChildDecl {
            name: "childA".to_string(),
            url: "ignored:///child".to_string(),
            startup: fsys::StartupMode::Lazy,
            environment: None,
        };
        let child_b = ChildDecl {
            name: "childB".to_string(),
            url: "ignored:///child".to_string(),
            startup: fsys::StartupMode::Lazy,
            environment: None,
        };
        let child_c = ChildDecl {
            name: "childC".to_string(),
            url: "ignored:///child".to_string(),
            startup: fsys::StartupMode::Lazy,
            environment: None,
        };
        let decl = ComponentDecl {
            offers: vec![
                OfferDecl::Protocol(OfferProtocolDecl {
                    source: OfferSource::Child("childB".to_string()),
                    source_name: "childBOffer".into(),
                    target_name: "serviceSibling".into(),
                    target: OfferTarget::Child("childA".to_string()),
                    dependency_type: DependencyType::Strong,
                }),
                OfferDecl::Protocol(OfferProtocolDecl {
                    source: OfferSource::Child("childB".to_string()),
                    source_name: "childBToC".into(),
                    target_name: "serviceSibling".into(),
                    target: OfferTarget::Child("childC".to_string()),
                    dependency_type: DependencyType::Strong,
                }),
            ],
            children: vec![child_a.clone(), child_b.clone(), child_c.clone()],

            ..default_component_decl()
        };

        let mut expected: Vec<(DependencyNode, Vec<DependencyNode>)> = Vec::new();
        let mut v = vec![
            DependencyNode::Child(child_a.name.clone()),
            DependencyNode::Child(child_c.name.clone()),
        ];
        v.sort_unstable();
        expected.push((
            DependencyNode::Parent,
            vec![
                DependencyNode::Child(child_a.name.clone()),
                DependencyNode::Child(child_b.name.clone()),
                DependencyNode::Child(child_c.name.clone()),
            ],
        ));
        expected.push((DependencyNode::Child(child_b.name.clone()), v));
        expected.push((DependencyNode::Child(child_a.name.clone()), vec![]));
        expected.push((DependencyNode::Child(child_c.name.clone()), vec![]));
        expected.sort_unstable();
        validate_results(expected, process_component_dependencies(&decl));
    }

    #[test_case(DependencyType::Weak)]
    #[test_case(DependencyType::WeakForMigration)]
    fn test_multiple_dependencies(weak_dep: DependencyType) {
        let child_a = ChildDecl {
            name: "childA".to_string(),
            url: "ignored:///child".to_string(),
            startup: fsys::StartupMode::Lazy,
            environment: None,
        };
        let child_b = ChildDecl {
            name: "childB".to_string(),
            url: "ignored:///child".to_string(),
            startup: fsys::StartupMode::Lazy,
            environment: None,
        };
        let child_c = ChildDecl {
            name: "childC".to_string(),
            url: "ignored:///child".to_string(),
            startup: fsys::StartupMode::Lazy,
            environment: None,
        };
        let decl = ComponentDecl {
            offers: vec![
                OfferDecl::Protocol(OfferProtocolDecl {
                    source: OfferSource::Child("childA".to_string()),
                    source_name: "childBOffer".into(),
                    target_name: "serviceSibling".into(),
                    target: OfferTarget::Child("childC".to_string()),
                    dependency_type: DependencyType::Strong,
                }),
                OfferDecl::Protocol(OfferProtocolDecl {
                    source: OfferSource::Child("childB".to_string()),
                    source_name: "childBToC".into(),
                    target_name: "serviceSibling".into(),
                    target: OfferTarget::Child("childC".to_string()),
                    dependency_type: DependencyType::Strong,
                }),
                OfferDecl::Protocol(OfferProtocolDecl {
                    source: OfferSource::Child("childC".to_string()),
                    source_name: "childCToA".into(),
                    target_name: "serviceSibling".into(),
                    target: OfferTarget::Child("childA".to_string()),
                    dependency_type: weak_dep,
                }),
            ],
            children: vec![child_a.clone(), child_b.clone(), child_c.clone()],
            ..default_component_decl()
        };

        let mut expected: Vec<(DependencyNode, Vec<DependencyNode>)> = Vec::new();
        expected.push((
            DependencyNode::Parent,
            vec![
                DependencyNode::Child(child_a.name.clone()),
                DependencyNode::Child(child_b.name.clone()),
                DependencyNode::Child(child_c.name.clone()),
            ],
        ));
        expected.push((
            DependencyNode::Child(child_b.name.clone()),
            vec![DependencyNode::Child(child_c.name.clone())],
        ));
        expected.push((
            DependencyNode::Child(child_a.name.clone()),
            vec![DependencyNode::Child(child_c.name.clone())],
        ));
        expected.push((DependencyNode::Child(child_c.name.clone()), vec![]));
        expected.sort_unstable();

        validate_results(expected, process_component_dependencies(&decl));
    }

    #[test]
    fn test_component_is_source_and_target() {
        let child_a = ChildDecl {
            name: "childA".to_string(),
            url: "ignored:///child".to_string(),
            startup: fsys::StartupMode::Lazy,
            environment: None,
        };
        let child_b = ChildDecl {
            name: "childB".to_string(),
            url: "ignored:///child".to_string(),
            startup: fsys::StartupMode::Lazy,
            environment: None,
        };
        let child_c = ChildDecl {
            name: "childC".to_string(),
            url: "ignored:///child".to_string(),
            startup: fsys::StartupMode::Lazy,
            environment: None,
        };
        let decl = ComponentDecl {
            offers: vec![
                OfferDecl::Protocol(OfferProtocolDecl {
                    source: OfferSource::Child("childA".to_string()),
                    source_name: "childBOffer".into(),
                    target_name: "serviceSibling".into(),
                    target: OfferTarget::Child("childB".to_string()),
                    dependency_type: DependencyType::Strong,
                }),
                OfferDecl::Protocol(OfferProtocolDecl {
                    source: OfferSource::Child("childB".to_string()),
                    source_name: "childBToC".into(),
                    target_name: "serviceSibling".into(),
                    target: OfferTarget::Child("childC".to_string()),
                    dependency_type: DependencyType::Strong,
                }),
            ],
            children: vec![child_a.clone(), child_b.clone(), child_c.clone()],
            ..default_component_decl()
        };

        let mut expected: Vec<(DependencyNode, Vec<DependencyNode>)> = Vec::new();
        expected.push((
            DependencyNode::Parent,
            vec![
                DependencyNode::Child(child_a.name.clone()),
                DependencyNode::Child(child_b.name.clone()),
                DependencyNode::Child(child_c.name.clone()),
            ],
        ));
        expected.push((
            DependencyNode::Child(child_a.name.clone()),
            vec![DependencyNode::Child(child_b.name.clone())],
        ));
        expected.push((
            DependencyNode::Child(child_b.name.clone()),
            vec![DependencyNode::Child(child_c.name.clone())],
        ));
        expected.push((DependencyNode::Child(child_c.name.clone()), vec![]));
        expected.sort_unstable();
        validate_results(expected, process_component_dependencies(&decl));
    }

    /// Tests a graph that looks like the below, tildes indicate a
    /// capability route. Route point toward the target of the capability
    /// offer. The manifest constructed is for 'P'.
    ///       P
    ///    ___|___
    ///  /  / | \  \
    /// e<~c<~a~>b~>d
    ///     \      /
    ///      *>~~>*
    #[test]
    fn test_complex_routing() {
        let child_a = ChildDecl {
            name: "childA".to_string(),
            url: "ignored:///child".to_string(),
            startup: fsys::StartupMode::Lazy,
            environment: None,
        };
        let child_b = ChildDecl {
            name: "childB".to_string(),
            url: "ignored:///child".to_string(),
            startup: fsys::StartupMode::Lazy,
            environment: None,
        };
        let child_c = ChildDecl {
            name: "childC".to_string(),
            url: "ignored:///child".to_string(),
            startup: fsys::StartupMode::Lazy,
            environment: None,
        };
        let child_d = ChildDecl {
            name: "childD".to_string(),
            url: "ignored:///child".to_string(),
            startup: fsys::StartupMode::Lazy,
            environment: None,
        };
        let child_e = ChildDecl {
            name: "childE".to_string(),
            url: "ignored:///child".to_string(),
            startup: fsys::StartupMode::Lazy,
            environment: None,
        };
        let decl = ComponentDecl {
            offers: vec![
                OfferDecl::Protocol(OfferProtocolDecl {
                    source: OfferSource::Child("childA".to_string()),
                    source_name: "childAService".into(),
                    target_name: "childAService".into(),
                    target: OfferTarget::Child("childB".to_string()),
                    dependency_type: DependencyType::Strong,
                }),
                OfferDecl::Protocol(OfferProtocolDecl {
                    source: OfferSource::Child("childA".to_string()),
                    source_name: "childAService".into(),
                    target_name: "childAService".into(),
                    target: OfferTarget::Child("childC".to_string()),
                    dependency_type: DependencyType::Strong,
                }),
                OfferDecl::Protocol(OfferProtocolDecl {
                    source: OfferSource::Child("childB".to_string()),
                    source_name: "childBService".into(),
                    target_name: "childBService".into(),
                    target: OfferTarget::Child("childD".to_string()),
                    dependency_type: DependencyType::Strong,
                }),
                OfferDecl::Protocol(OfferProtocolDecl {
                    source: OfferSource::Child("childC".to_string()),
                    source_name: "childAService".into(),
                    target_name: "childAService".into(),
                    target: OfferTarget::Child("childD".to_string()),
                    dependency_type: DependencyType::Strong,
                }),
                OfferDecl::Protocol(OfferProtocolDecl {
                    source: OfferSource::Child("childC".to_string()),
                    source_name: "childAService".into(),
                    target_name: "childAService".into(),
                    target: OfferTarget::Child("childE".to_string()),
                    dependency_type: DependencyType::Strong,
                }),
            ],
            children: vec![
                child_a.clone(),
                child_b.clone(),
                child_c.clone(),
                child_d.clone(),
                child_e.clone(),
            ],
            ..default_component_decl()
        };

        let mut expected: Vec<(DependencyNode, Vec<DependencyNode>)> = Vec::new();
        expected.push((
            DependencyNode::Parent,
            vec![
                DependencyNode::Child(child_a.name.clone()),
                DependencyNode::Child(child_b.name.clone()),
                DependencyNode::Child(child_c.name.clone()),
                DependencyNode::Child(child_d.name.clone()),
                DependencyNode::Child(child_e.name.clone()),
            ],
        ));
        expected.push((
            DependencyNode::Child(child_a.name.clone()),
            vec![
                DependencyNode::Child(child_b.name.clone()),
                DependencyNode::Child(child_c.name.clone()),
            ],
        ));
        expected.push((
            DependencyNode::Child(child_b.name.clone()),
            vec![DependencyNode::Child(child_d.name.clone())],
        ));
        expected.push((
            DependencyNode::Child(child_c.name.clone()),
            vec![
                DependencyNode::Child(child_d.name.clone()),
                DependencyNode::Child(child_e.name.clone()),
            ],
        ));
        expected.push((DependencyNode::Child(child_d.name.clone()), vec![]));
        expected.push((DependencyNode::Child(child_e.name.clone()), vec![]));
        expected.sort_unstable();
        validate_results(expected, process_component_dependencies(&decl));
    }

    #[test]
    #[should_panic]
    fn test_target_does_not_exist() {
        let child_a = ChildDecl {
            name: "childA".to_string(),
            url: "ignored:///child".to_string(),
            startup: fsys::StartupMode::Lazy,
            environment: None,
        };
        // This declaration is invalid because the offer target doesn't exist
        let decl = ComponentDecl {
            offers: vec![OfferDecl::Protocol(OfferProtocolDecl {
                source: OfferSource::Child("childA".to_string()),
                source_name: "childBOffer".into(),
                target_name: "serviceSibling".into(),
                target: OfferTarget::Child("childB".to_string()),
                dependency_type: DependencyType::Strong,
            })],
            children: vec![child_a.clone()],
            ..default_component_decl()
        };

        process_component_dependencies(&decl);
    }

    #[test]
    #[should_panic]
    fn test_source_does_not_exist() {
        let child_a = ChildDecl {
            name: "childA".to_string(),
            url: "ignored:///child".to_string(),
            startup: fsys::StartupMode::Lazy,
            environment: None,
        };
        // This declaration is invalid because the offer target doesn't exist
        let decl = ComponentDecl {
            offers: vec![OfferDecl::Protocol(OfferProtocolDecl {
                source: OfferSource::Child("childB".to_string()),
                source_name: "childBOffer".into(),
                target_name: "serviceSibling".into(),
                target: OfferTarget::Child("childA".to_string()),
                dependency_type: DependencyType::Strong,
            })],
            children: vec![child_a.clone()],
            ..default_component_decl()
        };

        process_component_dependencies(&decl);
    }

    #[test]
    fn test_use_from_child() {
        let decl = ComponentDecl {
            offers: vec![OfferDecl::Protocol(OfferProtocolDecl {
                source: OfferSource::Self_,
                source_name: "serviceParent".into(),
                target_name: "serviceParent".into(),
                target: OfferTarget::Child("childA".to_string()),
                dependency_type: DependencyType::Strong,
            })],
            children: vec![ChildDecl {
                name: "childA".to_string(),
                url: "ignored:///child".to_string(),
                startup: fsys::StartupMode::Lazy,
                environment: None,
            }],
            uses: vec![UseDecl::Protocol(UseProtocolDecl {
                source: UseSource::Child("childA".to_string()),
                source_name: "test.protocol".into(),
                target_path: CapabilityPath::try_from("/svc/test.protocol").unwrap(),
                dependency_type: DependencyType::Strong,
            })],
            ..default_component_decl()
        };

        let mut expected: Vec<(DependencyNode, Vec<DependencyNode>)> = Vec::new();
        expected.push((DependencyNode::Parent, vec![]));
        expected.push((DependencyNode::Child("childA".to_string()), vec![DependencyNode::Parent]));
        validate_results(expected, process_component_dependencies(&decl));
    }

    #[test]
    fn test_use_from_some_children() {
        let decl = ComponentDecl {
            offers: vec![OfferDecl::Protocol(OfferProtocolDecl {
                source: OfferSource::Self_,
                source_name: "serviceParent".into(),
                target_name: "serviceParent".into(),
                target: OfferTarget::Child("childA".to_string()),
                dependency_type: DependencyType::Strong,
            })],
            children: vec![
                ChildDecl {
                    name: "childA".to_string(),
                    url: "ignored:///child".to_string(),
                    startup: fsys::StartupMode::Lazy,
                    environment: None,
                },
                ChildDecl {
                    name: "childB".to_string(),
                    url: "ignored:///child".to_string(),
                    startup: fsys::StartupMode::Lazy,
                    environment: None,
                },
            ],
            uses: vec![UseDecl::Protocol(UseProtocolDecl {
                source: UseSource::Child("childA".to_string()),
                source_name: "test.protocol".into(),
                target_path: CapabilityPath::try_from("/svc/test.protocol").unwrap(),
                dependency_type: DependencyType::Strong,
            })],
            ..default_component_decl()
        };

        let mut expected: Vec<(DependencyNode, Vec<DependencyNode>)> = Vec::new();
        // childB is a dependent because we consider all children dependent, unless the parent
        // uses something from the child.
        expected.push((DependencyNode::Parent, vec![DependencyNode::Child("childB".to_string())]));
        expected.push((DependencyNode::Child("childA".to_string()), vec![DependencyNode::Parent]));
        expected.push((DependencyNode::Child("childB".to_string()), vec![]));
        validate_results(expected, process_component_dependencies(&decl));
    }

    #[test]
    fn test_use_from_child_weak() {
        let decl = ComponentDecl {
            offers: vec![OfferDecl::Protocol(OfferProtocolDecl {
                source: OfferSource::Self_,
                source_name: "serviceParent".into(),
                target_name: "serviceParent".into(),
                target: OfferTarget::Child("childA".to_string()),
                dependency_type: DependencyType::Strong,
            })],
            children: vec![ChildDecl {
                name: "childA".to_string(),
                url: "ignored:///child".to_string(),
                startup: fsys::StartupMode::Lazy,
                environment: None,
            }],
            uses: vec![UseDecl::Protocol(UseProtocolDecl {
                source: UseSource::Child("childA".to_string()),
                source_name: "test.protocol".into(),
                target_path: CapabilityPath::try_from("/svc/test.protocol").unwrap(),
                dependency_type: DependencyType::Weak,
            })],
            ..default_component_decl()
        };

        let mut expected: Vec<(DependencyNode, Vec<DependencyNode>)> = Vec::new();
        expected.push((DependencyNode::Parent, vec![DependencyNode::Child("childA".to_string())]));
        expected.push((DependencyNode::Child("childA".to_string()), vec![]));
        validate_results(expected, process_component_dependencies(&decl));
    }

    #[test]
    fn test_use_from_some_children_weak() {
        let decl = ComponentDecl {
            offers: vec![OfferDecl::Protocol(OfferProtocolDecl {
                source: OfferSource::Self_,
                source_name: "serviceParent".into(),
                target_name: "serviceParent".into(),
                target: OfferTarget::Child("childA".to_string()),
                dependency_type: DependencyType::Strong,
            })],
            children: vec![
                ChildDecl {
                    name: "childA".to_string(),
                    url: "ignored:///child".to_string(),
                    startup: fsys::StartupMode::Lazy,
                    environment: None,
                },
                ChildDecl {
                    name: "childB".to_string(),
                    url: "ignored:///child".to_string(),
                    startup: fsys::StartupMode::Lazy,
                    environment: None,
                },
            ],
            uses: vec![
                UseDecl::Protocol(UseProtocolDecl {
                    source: UseSource::Child("childA".to_string()),
                    source_name: "test.protocol".into(),
                    target_path: CapabilityPath::try_from("/svc/test.protocol").unwrap(),
                    dependency_type: DependencyType::Strong,
                }),
                UseDecl::Protocol(UseProtocolDecl {
                    source: UseSource::Child("childB".to_string()),
                    source_name: "test.protocol2".into(),
                    target_path: CapabilityPath::try_from("/svc/test.protocol2").unwrap(),
                    dependency_type: DependencyType::Weak,
                }),
            ],
            ..default_component_decl()
        };

        let mut expected: Vec<(DependencyNode, Vec<DependencyNode>)> = Vec::new();
        // childB is a dependent because its use-from-child has a 'weak' dependency.
        expected.push((DependencyNode::Parent, vec![DependencyNode::Child("childB".to_string())]));
        expected.push((DependencyNode::Child("childA".to_string()), vec![DependencyNode::Parent]));
        expected.push((DependencyNode::Child("childB".to_string()), vec![]));
        validate_results(expected, process_component_dependencies(&decl));
    }

    #[test]
    fn test_resolver_capability_creates_dependency() {
        let child_a = ChildDecl {
            name: "childA".to_string(),
            url: "ignored:///child".to_string(),
            startup: fsys::StartupMode::Lazy,
            environment: None,
        };
        let child_b = ChildDecl {
            name: "childB".to_string(),
            url: "ignored:///child".to_string(),
            startup: fsys::StartupMode::Lazy,
            environment: None,
        };
        let decl = ComponentDecl {
            offers: vec![OfferDecl::Resolver(OfferResolverDecl {
                source: OfferSource::Child("childA".to_string()),
                source_name: CapabilityName::try_from("resolver").unwrap(),
                target_name: CapabilityName::try_from("resolver").unwrap(),
                target: OfferTarget::Child("childB".to_string()),
            })],
            children: vec![child_a.clone(), child_b.clone()],
            ..default_component_decl()
        };

        let mut expected = vec![
            (
                DependencyNode::Parent,
                vec![
                    DependencyNode::Child(child_a.name.clone()),
                    DependencyNode::Child(child_b.name.clone()),
                ],
            ),
            (
                DependencyNode::Child(child_a.name.clone()),
                vec![DependencyNode::Child(child_b.name.clone())],
            ),
            (DependencyNode::Child(child_b.name.clone()), vec![]),
        ];
        expected.sort_unstable();
        validate_results(expected, process_component_dependencies(&decl));
    }

    #[fuchsia::test]
    async fn action_shutdown_blocks_stop() {
        let test = ActionsTest::new("root", vec![], None).await;
        let component = test.model.root.clone();
        let mut action_set = component.lock_actions().await;

        // Register some actions, and get notifications. Use `register_inner` so we can register
        // the action without immediately running it.
        let (task1, nf1) = action_set.register_inner(&component, ShutdownAction::new());
        let (task2, nf2) = action_set.register_inner(&component, StopAction::new());

        drop(action_set);

        // Complete actions, while checking futures.
        ActionSet::finish(&component, &ActionKey::Shutdown).await;

        // nf2 should be blocked on task1 completing.
        assert!(nf1.fut.peek().is_none());
        assert!(nf2.fut.peek().is_none());
        task1.unwrap().tx.send(Ok(())).unwrap();
        task2.unwrap().spawn();
        nf1.await.unwrap();
        nf2.await.unwrap();
    }

    #[fuchsia::test]
    async fn action_shutdown_stop_stop() {
        let test = ActionsTest::new("root", vec![], None).await;
        let component = test.model.root.clone();
        let mut action_set = component.lock_actions().await;

        // Register some actions, and get notifications. Use `register_inner` so we can register
        // the action without immediately running it.
        let (task1, nf1) = action_set.register_inner(&component, ShutdownAction::new());
        let (task2, nf2) = action_set.register_inner(&component, StopAction::new());
        let (task3, nf3) = action_set.register_inner(&component, StopAction::new());

        drop(action_set);

        // Complete actions, while checking notifications.
        ActionSet::finish(&component, &ActionKey::Shutdown).await;

        // nf2 and nf3 should be blocked on task1 completing.
        assert!(nf1.fut.peek().is_none());
        assert!(nf2.fut.peek().is_none());
        task1.unwrap().tx.send(Ok(())).unwrap();
        task2.unwrap().spawn();
        assert!(task3.is_none());
        nf1.await.unwrap();
        nf2.await.unwrap();
        nf3.await.unwrap();
    }

    #[fuchsia::test]
    async fn shutdown_one_component() {
        let components = vec![
            ("root", ComponentDeclBuilder::new().add_lazy_child("a").build()),
            ("a", component_decl_with_test_runner()),
        ];
        let test = ActionsTest::new("root", components, None).await;
        // Bind to the component, causing it to start. This should cause the component to have an
        // `Execution`.
        let component = test.look_up(vec!["a:0"].into()).await;
        test.model
            .bind(&component.abs_moniker, &BindReason::Eager)
            .await
            .expect("could not bind to a");
        assert!(is_executing(&component).await);
        let a_info = ComponentInfo::new(component.clone()).await;

        // Register shutdown action, and wait for it. Component should shut down (no more
        // `Execution`).
        ActionSet::register(a_info.component.clone(), ShutdownAction::new())
            .await
            .expect("shutdown failed");
        a_info.check_is_shut_down(&test.runner).await;

        // Trying to bind to the component should fail because it's shut down.
        test.model
            .bind(&a_info.component.abs_moniker, &BindReason::Eager)
            .await
            .expect_err("successfully bound to a after shutdown");

        // Shut down the component again. This succeeds, but has no additional effect.
        ActionSet::register(a_info.component.clone(), ShutdownAction::new())
            .await
            .expect("shutdown failed");
        &a_info.check_is_shut_down(&test.runner).await;
    }

    #[fuchsia::test]
    async fn shutdown_collection() {
        let components = vec![
            ("root", ComponentDeclBuilder::new().add_lazy_child("container").build()),
            (
                "container",
                ComponentDeclBuilder::new()
                    .add_transient_collection("coll")
                    .add_lazy_child("c")
                    .build(),
            ),
            ("a", component_decl_with_test_runner()),
            ("b", component_decl_with_test_runner()),
            ("c", component_decl_with_test_runner()),
        ];
        let test = ActionsTest::new("root", components, Some(vec!["container:0"].into())).await;

        // Create dynamic instances in "coll".
        test.create_dynamic_child("coll", "a").await;
        test.create_dynamic_child("coll", "b").await;

        // Bind to the components, causing them to start. This should cause them to have an
        // `Execution`.
        let component_container = test.look_up(vec!["container:0"].into()).await;
        let component_a = test.look_up(vec!["container:0", "coll:a:1"].into()).await;
        let component_b = test.look_up(vec!["container:0", "coll:b:2"].into()).await;
        let component_c = test.look_up(vec!["container:0", "c:0"].into()).await;
        test.model
            .bind(&component_container.abs_moniker, &BindReason::Eager)
            .await
            .expect("could not bind to container");
        test.model
            .bind(&component_a.abs_moniker, &BindReason::Eager)
            .await
            .expect("could not bind to coll:a");
        test.model
            .bind(&component_b.abs_moniker, &BindReason::Eager)
            .await
            .expect("could not bind to coll:b");
        test.model
            .bind(&component_c.abs_moniker, &BindReason::Eager)
            .await
            .expect("could not bind to coll:b");
        assert!(is_executing(&component_container).await);
        assert!(is_executing(&component_a).await);
        assert!(is_executing(&component_b).await);
        assert!(is_executing(&component_c).await);
        assert!(has_child(&component_container, "coll:a:1").await);
        assert!(has_child(&component_container, "coll:b:2").await);

        let component_a_info = ComponentInfo::new(component_a).await;
        let component_b_info = ComponentInfo::new(component_b).await;
        let component_container_info = ComponentInfo::new(component_container).await;

        // Register shutdown action, and wait for it. Components should shut down (no more
        // `Execution`). Also, the instances in the collection should have been destroyed because
        // they were transient.
        ActionSet::register(component_container_info.component.clone(), ShutdownAction::new())
            .await
            .expect("shutdown failed");
        component_container_info.check_is_shut_down(&test.runner).await;
        assert!(!has_child(&component_container_info.component, "coll:a:1").await);
        assert!(!has_child(&component_container_info.component, "coll:b:2").await);
        assert!(has_child(&component_container_info.component, "c:0").await);
        component_a_info.check_is_shut_down(&test.runner).await;
        component_b_info.check_is_shut_down(&test.runner).await;

        // Verify events.
        {
            let mut events: Vec<_> = test
                .test_hook
                .lifecycle()
                .into_iter()
                .filter(|e| match e {
                    Lifecycle::Stop(_) | Lifecycle::Destroy(_) => true,
                    _ => false,
                })
                .collect();
            // The leaves could be stopped in any order.
            let mut next: Vec<_> = events.drain(0..3).collect();
            next.sort_unstable();
            let expected: Vec<_> = vec![
                Lifecycle::Stop(vec!["container:0", "c:0"].into()),
                Lifecycle::Stop(vec!["container:0", "coll:a:1"].into()),
                Lifecycle::Stop(vec!["container:0", "coll:b:2"].into()),
            ];
            assert_eq!(next, expected);

            // These components were destroyed because they lived in a transient collection.
            let mut next: Vec<_> = events.drain(0..2).collect();
            next.sort_unstable();
            let expected: Vec<_> = vec![
                Lifecycle::Destroy(vec!["container:0", "coll:a:1"].into()),
                Lifecycle::Destroy(vec!["container:0", "coll:b:2"].into()),
            ];
            assert_eq!(next, expected);
        }
    }

    #[fuchsia::test]
    async fn shutdown_not_started() {
        let components = vec![
            ("root", ComponentDeclBuilder::new().add_lazy_child("a").build()),
            ("a", ComponentDeclBuilder::new().add_lazy_child("b").build()),
            ("b", component_decl_with_test_runner()),
        ];
        let test = ActionsTest::new("root", components, None).await;
        let component_a = test.look_up(vec!["a:0"].into()).await;
        let component_b = test.look_up(vec!["a:0", "b:0"].into()).await;
        assert!(!is_executing(&component_a).await);
        assert!(!is_executing(&component_b).await);

        // Register shutdown action on "a", and wait for it.
        ActionSet::register(component_a.clone(), ShutdownAction::new())
            .await
            .expect("shutdown failed");
        assert!(execution_is_shut_down(&component_a).await);
        assert!(execution_is_shut_down(&component_b).await);

        // Now "a" is shut down. There should be no events though because the component was
        // never started.
        ActionSet::register(component_a.clone(), ShutdownAction::new())
            .await
            .expect("shutdown failed");
        assert!(execution_is_shut_down(&component_a).await);
        assert!(execution_is_shut_down(&component_b).await);
        {
            let events: Vec<_> = test
                .test_hook
                .lifecycle()
                .into_iter()
                .filter(|e| match e {
                    Lifecycle::Stop(_) => true,
                    _ => false,
                })
                .collect();
            assert_eq!(events, Vec::<Lifecycle>::new());
        }
    }

    #[fuchsia::test]
    async fn shutdown_not_resolved() {
        let components = vec![
            ("root", ComponentDeclBuilder::new().add_lazy_child("a").build()),
            ("a", ComponentDeclBuilder::new().add_lazy_child("b").build()),
            ("b", ComponentDeclBuilder::new().add_lazy_child("c").build()),
            ("c", component_decl_with_test_runner()),
        ];
        let test = ActionsTest::new("root", components, None).await;
        let component_a = test.look_up(vec!["a:0"].into()).await;
        test.model
            .bind(&component_a.abs_moniker, &BindReason::Eager)
            .await
            .expect("could not bind to a");
        assert!(is_executing(&component_a).await);

        // Register shutdown action on "a", and wait for it.
        ActionSet::register(component_a.clone(), ShutdownAction::new())
            .await
            .expect("shutdown failed");
        assert!(execution_is_shut_down(&component_a).await);
        // Get component without resolving it.
        let component_b = {
            let state = component_a.lock_state().await;
            match *state {
                InstanceState::Resolved(ref s) => {
                    s.get_live_child(&PartialChildMoniker::from("b")).expect("child b not found")
                }
                _ => panic!("not resolved"),
            }
        };
        assert!(execution_is_shut_down(&component_b).await);
        assert!(is_unresolved(&component_b).await);

        // Now "a" is shut down. There should be no event for "b" because it was never started
        // (or resolved).
        {
            let events: Vec<_> = test
                .test_hook
                .lifecycle()
                .into_iter()
                .filter(|e| match e {
                    Lifecycle::Stop(_) => true,
                    _ => false,
                })
                .collect();
            assert_eq!(events, vec![Lifecycle::Stop(vec!["a:0"].into())]);
        }
    }

    /// Shut down `a`:
    ///  a
    ///   \
    ///    b
    ///   / \
    ///  c   d
    #[fuchsia::test]
    async fn shutdown_hierarchy() {
        let components = vec![
            ("root", ComponentDeclBuilder::new().add_lazy_child("a").build()),
            ("a", ComponentDeclBuilder::new().add_eager_child("b").build()),
            ("b", ComponentDeclBuilder::new().add_eager_child("c").add_eager_child("d").build()),
            ("c", component_decl_with_test_runner()),
            ("d", component_decl_with_test_runner()),
        ];
        let test = ActionsTest::new("root", components, None).await;
        let component_a = test.look_up(vec!["a:0"].into()).await;
        let component_b = test.look_up(vec!["a:0", "b:0"].into()).await;
        let component_c = test.look_up(vec!["a:0", "b:0", "c:0"].into()).await;
        let component_d = test.look_up(vec!["a:0", "b:0", "d:0"].into()).await;

        // Component startup was eager, so they should all have an `Execution`.
        test.model
            .bind(&component_a.abs_moniker, &BindReason::Eager)
            .await
            .expect("could not bind to a");
        assert!(is_executing(&component_a).await);
        assert!(is_executing(&component_b).await);
        assert!(is_executing(&component_c).await);
        assert!(is_executing(&component_d).await);

        let component_a_info = ComponentInfo::new(component_a).await;
        let component_b_info = ComponentInfo::new(component_b).await;
        let component_c_info = ComponentInfo::new(component_c).await;
        let component_d_info = ComponentInfo::new(component_d).await;

        // Register shutdown action on "a", and wait for it. This should cause all components
        // to shut down, in bottom-up order.
        ActionSet::register(component_a_info.component.clone(), ShutdownAction::new())
            .await
            .expect("shutdown failed");
        component_a_info.check_is_shut_down(&test.runner).await;
        component_b_info.check_is_shut_down(&test.runner).await;
        component_c_info.check_is_shut_down(&test.runner).await;
        component_d_info.check_is_shut_down(&test.runner).await;
        {
            let mut events: Vec<_> = test
                .test_hook
                .lifecycle()
                .into_iter()
                .filter(|e| match e {
                    Lifecycle::Stop(_) => true,
                    _ => false,
                })
                .collect();
            let mut first: Vec<_> = events.drain(0..2).collect();
            first.sort_unstable();
            let expected: Vec<_> = vec![
                Lifecycle::Stop(vec!["a:0", "b:0", "c:0"].into()),
                Lifecycle::Stop(vec!["a:0", "b:0", "d:0"].into()),
            ];
            assert_eq!(first, expected);
            assert_eq!(
                events,
                vec![
                    Lifecycle::Stop(vec!["a:0", "b:0"].into()),
                    Lifecycle::Stop(vec!["a:0"].into())
                ]
            );
        }
    }

    /// Shut down `a`:
    ///   a
    ///    \
    ///     b
    ///   / | \
    ///  c<-d->e
    /// In this case C and E use a service provided by d
    #[fuchsia::test]
    async fn shutdown_with_multiple_deps() {
        let components = vec![
            ("root", ComponentDeclBuilder::new().add_lazy_child("a").build()),
            ("a", ComponentDeclBuilder::new().add_eager_child("b").build()),
            (
                "b",
                ComponentDeclBuilder::new()
                    .add_eager_child("c")
                    .add_eager_child("d")
                    .add_eager_child("e")
                    .offer(OfferDecl::Protocol(OfferProtocolDecl {
                        source: OfferSource::Child("d".to_string()),
                        source_name: "serviceD".into(),
                        target_name: "serviceD".into(),
                        target: OfferTarget::Child("c".to_string()),
                        dependency_type: DependencyType::Strong,
                    }))
                    .offer(OfferDecl::Protocol(OfferProtocolDecl {
                        source: OfferSource::Child("d".to_string()),
                        source_name: "serviceD".into(),
                        target_name: "serviceD".into(),
                        target: OfferTarget::Child("e".to_string()),
                        dependency_type: DependencyType::Strong,
                    }))
                    .build(),
            ),
            (
                "c",
                ComponentDeclBuilder::new()
                    .use_(UseDecl::Protocol(UseProtocolDecl {
                        source: UseSource::Parent,
                        source_name: "serviceD".into(),
                        target_path: CapabilityPath::try_from("/svc/serviceD").unwrap(),
                        dependency_type: DependencyType::Strong,
                    }))
                    .build(),
            ),
            (
                "d",
                ComponentDeclBuilder::new()
                    .protocol(ProtocolDecl {
                        name: "serviceD".into(),
                        source_path: "/svc/serviceD".parse().unwrap(),
                    })
                    .expose(ExposeDecl::Protocol(ExposeProtocolDecl {
                        source: ExposeSource::Self_,
                        source_name: "serviceD".into(),
                        target_name: "serviceD".into(),
                        target: ExposeTarget::Parent,
                    }))
                    .build(),
            ),
            (
                "e",
                ComponentDeclBuilder::new()
                    .use_(UseDecl::Protocol(UseProtocolDecl {
                        source: UseSource::Parent,
                        source_name: "serviceD".into(),
                        target_path: CapabilityPath::try_from("/svc/serviceD").unwrap(),
                        dependency_type: DependencyType::Strong,
                    }))
                    .build(),
            ),
        ];
        let test = ActionsTest::new("root", components, None).await;
        let component_a = test.look_up(vec!["a:0"].into()).await;
        let component_b = test.look_up(vec!["a:0", "b:0"].into()).await;
        let component_c = test.look_up(vec!["a:0", "b:0", "c:0"].into()).await;
        let component_d = test.look_up(vec!["a:0", "b:0", "d:0"].into()).await;
        let component_e = test.look_up(vec!["a:0", "b:0", "e:0"].into()).await;

        // Component startup was eager, so they should all have an `Execution`.
        test.model
            .bind(&component_a.abs_moniker, &BindReason::Eager)
            .await
            .expect("could not bind to a");
        assert!(is_executing(&component_a).await);
        assert!(is_executing(&component_b).await);
        assert!(is_executing(&component_c).await);
        assert!(is_executing(&component_d).await);
        assert!(is_executing(&component_e).await);

        let component_a_info = ComponentInfo::new(component_a).await;
        let component_b_info = ComponentInfo::new(component_b).await;
        let component_c_info = ComponentInfo::new(component_c).await;
        let component_d_info = ComponentInfo::new(component_d).await;
        let component_e_info = ComponentInfo::new(component_e).await;

        // Register shutdown action on "a", and wait for it. This should cause all components
        // to shut down, in bottom-up order.
        ActionSet::register(component_a_info.component.clone(), ShutdownAction::new())
            .await
            .expect("shutdown failed");
        component_a_info.check_is_shut_down(&test.runner).await;
        component_b_info.check_is_shut_down(&test.runner).await;
        component_c_info.check_is_shut_down(&test.runner).await;
        component_d_info.check_is_shut_down(&test.runner).await;
        component_e_info.check_is_shut_down(&test.runner).await;

        {
            let mut events: Vec<_> = test
                .test_hook
                .lifecycle()
                .into_iter()
                .filter(|e| match e {
                    Lifecycle::Stop(_) => true,
                    _ => false,
                })
                .collect();
            let mut first: Vec<_> = events.drain(0..2).collect();
            first.sort_unstable();
            let mut expected: Vec<_> = vec![
                Lifecycle::Stop(vec!["a:0", "b:0", "c:0"].into()),
                Lifecycle::Stop(vec!["a:0", "b:0", "e:0"].into()),
            ];
            assert_eq!(first, expected);

            let next: Vec<_> = events.drain(0..1).collect();
            expected = vec![Lifecycle::Stop(vec!["a:0", "b:0", "d:0"].into())];
            assert_eq!(next, expected);

            assert_eq!(
                events,
                vec![
                    Lifecycle::Stop(vec!["a:0", "b:0"].into()),
                    Lifecycle::Stop(vec!["a:0"].into())
                ]
            );
        }
    }

    /// Shut down `a`:
    ///    a
    ///     \
    ///      b
    ///   / / \  \
    ///  c<-d->e->f
    /// In this case C and E use a service provided by D and
    /// F uses a service provided by E, shutdown order should be
    /// {F}, {C, E}, {D}, {B}, {A}
    /// Note that C must stop before D, but may stop before or after
    /// either of F and E.
    #[fuchsia::test]
    async fn shutdown_with_multiple_out_and_longer_chain() {
        let components = vec![
            ("root", ComponentDeclBuilder::new().add_lazy_child("a").build()),
            ("a", ComponentDeclBuilder::new().add_eager_child("b").build()),
            (
                "b",
                ComponentDeclBuilder::new()
                    .add_eager_child("c")
                    .add_eager_child("d")
                    .add_eager_child("e")
                    .add_eager_child("f")
                    .offer(OfferDecl::Protocol(OfferProtocolDecl {
                        source: OfferSource::Child("d".to_string()),
                        source_name: "serviceD".into(),
                        target_name: "serviceD".into(),
                        target: OfferTarget::Child("c".to_string()),
                        dependency_type: DependencyType::Strong,
                    }))
                    .offer(OfferDecl::Protocol(OfferProtocolDecl {
                        source: OfferSource::Child("d".to_string()),
                        source_name: "serviceD".into(),
                        target_name: "serviceD".into(),
                        target: OfferTarget::Child("e".to_string()),
                        dependency_type: DependencyType::Strong,
                    }))
                    .offer(OfferDecl::Protocol(OfferProtocolDecl {
                        source: OfferSource::Child("e".to_string()),
                        source_name: "serviceE".into(),
                        target_name: "serviceE".into(),
                        target: OfferTarget::Child("f".to_string()),
                        dependency_type: DependencyType::Strong,
                    }))
                    .build(),
            ),
            (
                "c",
                ComponentDeclBuilder::new()
                    .use_(UseDecl::Protocol(UseProtocolDecl {
                        source: UseSource::Parent,
                        source_name: "serviceD".into(),
                        target_path: CapabilityPath::try_from("/svc/serviceD").unwrap(),
                        dependency_type: DependencyType::Strong,
                    }))
                    .build(),
            ),
            (
                "d",
                ComponentDeclBuilder::new()
                    .protocol(ProtocolDecl {
                        name: "serviceD".into(),
                        source_path: "/svc/serviceD".parse().unwrap(),
                    })
                    .expose(ExposeDecl::Protocol(ExposeProtocolDecl {
                        source: ExposeSource::Self_,
                        source_name: "serviceD".into(),
                        target_name: "serviceD".into(),
                        target: ExposeTarget::Parent,
                    }))
                    .build(),
            ),
            (
                "e",
                ComponentDeclBuilder::new()
                    .protocol(ProtocolDecl {
                        name: "serviceE".into(),
                        source_path: "/svc/serviceE".parse().unwrap(),
                    })
                    .use_(UseDecl::Protocol(UseProtocolDecl {
                        source: UseSource::Parent,
                        source_name: "serviceD".into(),
                        target_path: CapabilityPath::try_from("/svc/serviceD").unwrap(),
                        dependency_type: DependencyType::Strong,
                    }))
                    .expose(ExposeDecl::Protocol(ExposeProtocolDecl {
                        source: ExposeSource::Self_,
                        source_name: "serviceE".into(),
                        target_name: "serviceE".into(),
                        target: ExposeTarget::Parent,
                    }))
                    .build(),
            ),
            (
                "f",
                ComponentDeclBuilder::new()
                    .use_(UseDecl::Protocol(UseProtocolDecl {
                        source: UseSource::Parent,
                        source_name: "serviceE".into(),
                        target_path: CapabilityPath::try_from("/svc/serviceE").unwrap(),
                        dependency_type: DependencyType::Strong,
                    }))
                    .build(),
            ),
        ];
        let moniker_a: AbsoluteMoniker = vec!["a:0"].into();
        let moniker_b: AbsoluteMoniker = vec!["a:0", "b:0"].into();
        let moniker_c: AbsoluteMoniker = vec!["a:0", "b:0", "c:0"].into();
        let moniker_d: AbsoluteMoniker = vec!["a:0", "b:0", "d:0"].into();
        let moniker_e: AbsoluteMoniker = vec!["a:0", "b:0", "e:0"].into();
        let moniker_f: AbsoluteMoniker = vec!["a:0", "b:0", "f:0"].into();
        let test = ActionsTest::new("root", components, None).await;
        let component_a = test.look_up(moniker_a.clone()).await;
        let component_b = test.look_up(moniker_b.clone()).await;
        let component_c = test.look_up(moniker_c.clone()).await;
        let component_d = test.look_up(moniker_d.clone()).await;
        let component_e = test.look_up(moniker_e.clone()).await;
        let component_f = test.look_up(moniker_f.clone()).await;

        // Component startup was eager, so they should all have an `Execution`.
        test.model
            .bind(&component_a.abs_moniker, &BindReason::Eager)
            .await
            .expect("could not bind to a");
        assert!(is_executing(&component_a).await);
        assert!(is_executing(&component_b).await);
        assert!(is_executing(&component_c).await);
        assert!(is_executing(&component_d).await);
        assert!(is_executing(&component_e).await);
        assert!(is_executing(&component_f).await);

        let component_a_info = ComponentInfo::new(component_a).await;
        let component_b_info = ComponentInfo::new(component_b).await;
        let component_c_info = ComponentInfo::new(component_c).await;
        let component_d_info = ComponentInfo::new(component_d).await;
        let component_e_info = ComponentInfo::new(component_e).await;
        let component_f_info = ComponentInfo::new(component_f).await;

        // Register shutdown action on "a", and wait for it. This should cause all components
        // to shut down, in bottom-up order.
        ActionSet::register(component_a_info.component.clone(), ShutdownAction::new())
            .await
            .expect("shutdown failed");
        component_a_info.check_is_shut_down(&test.runner).await;
        component_b_info.check_is_shut_down(&test.runner).await;
        component_c_info.check_is_shut_down(&test.runner).await;
        component_d_info.check_is_shut_down(&test.runner).await;
        component_e_info.check_is_shut_down(&test.runner).await;
        component_f_info.check_is_shut_down(&test.runner).await;

        let mut comes_after: HashMap<AbsoluteMoniker, Vec<AbsoluteMoniker>> = HashMap::new();
        comes_after.insert(moniker_a.clone(), vec![moniker_b.clone()]);
        // technically we could just depend on 'D' since it is the last of b's
        // children, but we add all the children for resilence against the
        // future
        comes_after.insert(
            moniker_b.clone(),
            vec![moniker_c.clone(), moniker_d.clone(), moniker_e.clone(), moniker_f.clone()],
        );
        comes_after.insert(moniker_d.clone(), vec![moniker_c.clone(), moniker_e.clone()]);
        comes_after.insert(moniker_c.clone(), vec![]);
        comes_after.insert(moniker_e.clone(), vec![moniker_f.clone()]);
        comes_after.insert(moniker_f.clone(), vec![]);
        {
            let events: Vec<_> = test
                .test_hook
                .lifecycle()
                .into_iter()
                .filter(|e| match e {
                    Lifecycle::Stop(_) => true,
                    _ => false,
                })
                .collect();

            for e in events {
                match e {
                    Lifecycle::Stop(moniker) => match comes_after.remove(&moniker) {
                        Some(dependents) => {
                            for d in dependents {
                                if comes_after.contains_key(&d) {
                                    panic!("{} stopped before its dependent {}", moniker, d);
                                }
                            }
                        }
                        None => {
                            panic!("{} was unknown or shut down more than once", moniker);
                        }
                    },
                    _ => {
                        panic!("Unexpected lifecycle type");
                    }
                }
            }
        }
    }

    /// Shut down `a`:
    ///           a
    ///
    ///           |
    ///
    ///     +---- b ----+
    ///    /             \
    ///   /     /   \     \
    ///
    ///  c <~~ d ~~> e ~~> f
    ///          \       /
    ///           +~~>~~+
    /// In this case C and E use a service provided by D and
    /// F uses a services provided by E and D, shutdown order should be F must
    /// stop before E and {C,E,F} must stop before D. C may stop before or
    /// after either of {F, E}.
    #[fuchsia::test]
    async fn shutdown_with_multiple_out_multiple_in() {
        let components = vec![
            ("root", ComponentDeclBuilder::new().add_lazy_child("a").build()),
            ("a", ComponentDeclBuilder::new().add_eager_child("b").build()),
            (
                "b",
                ComponentDeclBuilder::new()
                    .add_eager_child("c")
                    .add_eager_child("d")
                    .add_eager_child("e")
                    .add_eager_child("f")
                    .offer(OfferDecl::Protocol(OfferProtocolDecl {
                        source: OfferSource::Child("d".to_string()),
                        source_name: "serviceD".into(),
                        target_name: "serviceD".into(),
                        target: OfferTarget::Child("c".to_string()),
                        dependency_type: DependencyType::Strong,
                    }))
                    .offer(OfferDecl::Protocol(OfferProtocolDecl {
                        source: OfferSource::Child("d".to_string()),
                        source_name: "serviceD".into(),
                        target_name: "serviceD".into(),
                        target: OfferTarget::Child("e".to_string()),
                        dependency_type: DependencyType::Strong,
                    }))
                    .offer(OfferDecl::Protocol(OfferProtocolDecl {
                        source: OfferSource::Child("d".to_string()),
                        source_name: "serviceD".into(),
                        target_name: "serviceD".into(),
                        target: OfferTarget::Child("f".to_string()),
                        dependency_type: DependencyType::Strong,
                    }))
                    .offer(OfferDecl::Protocol(OfferProtocolDecl {
                        source: OfferSource::Child("e".to_string()),
                        source_name: "serviceE".into(),
                        target_name: "serviceE".into(),
                        target: OfferTarget::Child("f".to_string()),
                        dependency_type: DependencyType::Strong,
                    }))
                    .build(),
            ),
            (
                "c",
                ComponentDeclBuilder::new()
                    .use_(UseDecl::Protocol(UseProtocolDecl {
                        source: UseSource::Parent,
                        source_name: "serviceD".into(),
                        target_path: CapabilityPath::try_from("/svc/serviceD").unwrap(),
                        dependency_type: DependencyType::Strong,
                    }))
                    .build(),
            ),
            (
                "d",
                ComponentDeclBuilder::new()
                    .protocol(ProtocolDecl {
                        name: "serviceD".into(),
                        source_path: "/svc/serviceD".parse().unwrap(),
                    })
                    .expose(ExposeDecl::Protocol(ExposeProtocolDecl {
                        source: ExposeSource::Self_,
                        source_name: "serviceD".into(),
                        target_name: "serviceD".into(),
                        target: ExposeTarget::Parent,
                    }))
                    .build(),
            ),
            (
                "e",
                ComponentDeclBuilder::new()
                    .protocol(ProtocolDecl {
                        name: "serviceE".into(),
                        source_path: "/svc/serviceE".parse().unwrap(),
                    })
                    .use_(UseDecl::Protocol(UseProtocolDecl {
                        source: UseSource::Parent,
                        source_name: "serviceE".into(),
                        target_path: CapabilityPath::try_from("/svc/serviceE").unwrap(),
                        dependency_type: DependencyType::Strong,
                    }))
                    .expose(ExposeDecl::Protocol(ExposeProtocolDecl {
                        source: ExposeSource::Self_,
                        source_name: "serviceE".into(),
                        target_name: "serviceE".into(),
                        target: ExposeTarget::Parent,
                    }))
                    .build(),
            ),
            (
                "f",
                ComponentDeclBuilder::new()
                    .use_(UseDecl::Protocol(UseProtocolDecl {
                        source: UseSource::Parent,
                        source_name: "serviceE".into(),
                        target_path: CapabilityPath::try_from("/svc/serviceE").unwrap(),
                        dependency_type: DependencyType::Strong,
                    }))
                    .use_(UseDecl::Protocol(UseProtocolDecl {
                        source: UseSource::Parent,
                        source_name: "serviceD".into(),
                        target_path: CapabilityPath::try_from("/svc/serviceD").unwrap(),
                        dependency_type: DependencyType::Strong,
                    }))
                    .build(),
            ),
        ];
        let moniker_a: AbsoluteMoniker = vec!["a:0"].into();
        let moniker_b: AbsoluteMoniker = vec!["a:0", "b:0"].into();
        let moniker_c: AbsoluteMoniker = vec!["a:0", "b:0", "c:0"].into();
        let moniker_d: AbsoluteMoniker = vec!["a:0", "b:0", "d:0"].into();
        let moniker_e: AbsoluteMoniker = vec!["a:0", "b:0", "e:0"].into();
        let moniker_f: AbsoluteMoniker = vec!["a:0", "b:0", "f:0"].into();
        let test = ActionsTest::new("root", components, None).await;
        let component_a = test.look_up(moniker_a.clone()).await;
        let component_b = test.look_up(moniker_b.clone()).await;
        let component_c = test.look_up(moniker_c.clone()).await;
        let component_d = test.look_up(moniker_d.clone()).await;
        let component_e = test.look_up(moniker_e.clone()).await;
        let component_f = test.look_up(moniker_f.clone()).await;

        // Component startup was eager, so they should all have an `Execution`.
        test.model
            .bind(&component_a.abs_moniker, &BindReason::Eager)
            .await
            .expect("could not bind to a");
        assert!(is_executing(&component_a).await);
        assert!(is_executing(&component_b).await);
        assert!(is_executing(&component_c).await);
        assert!(is_executing(&component_d).await);
        assert!(is_executing(&component_e).await);
        assert!(is_executing(&component_f).await);

        let component_a_info = ComponentInfo::new(component_a).await;
        let component_b_info = ComponentInfo::new(component_b).await;
        let component_c_info = ComponentInfo::new(component_c).await;
        let component_d_info = ComponentInfo::new(component_d).await;
        let component_e_info = ComponentInfo::new(component_e).await;
        let component_f_info = ComponentInfo::new(component_f).await;

        // Register shutdown action on "a", and wait for it. This should cause all components
        // to shut down, in bottom-up order.
        ActionSet::register(component_a_info.component.clone(), ShutdownAction::new())
            .await
            .expect("shutdown failed");
        component_a_info.check_is_shut_down(&test.runner).await;
        component_b_info.check_is_shut_down(&test.runner).await;
        component_c_info.check_is_shut_down(&test.runner).await;
        component_d_info.check_is_shut_down(&test.runner).await;
        component_e_info.check_is_shut_down(&test.runner).await;
        component_f_info.check_is_shut_down(&test.runner).await;

        let mut comes_after: HashMap<AbsoluteMoniker, Vec<AbsoluteMoniker>> = HashMap::new();
        comes_after.insert(moniker_a.clone(), vec![moniker_b.clone()]);
        // technically we could just depend on 'D' since it is the last of b's
        // children, but we add all the children for resilence against the
        // future
        comes_after.insert(
            moniker_b.clone(),
            vec![moniker_c.clone(), moniker_d.clone(), moniker_e.clone(), moniker_f.clone()],
        );
        comes_after.insert(
            moniker_d.clone(),
            vec![moniker_c.clone(), moniker_e.clone(), moniker_f.clone()],
        );
        comes_after.insert(moniker_c.clone(), vec![]);
        comes_after.insert(moniker_e.clone(), vec![moniker_f.clone()]);
        comes_after.insert(moniker_f.clone(), vec![]);
        {
            let events: Vec<_> = test
                .test_hook
                .lifecycle()
                .into_iter()
                .filter(|e| match e {
                    Lifecycle::Stop(_) => true,
                    _ => false,
                })
                .collect();

            for e in events {
                match e {
                    Lifecycle::Stop(moniker) => {
                        let dependents = comes_after.remove(&moniker).expect(&format!(
                            "{} was unknown or shut down more than once",
                            moniker
                        ));
                        for d in dependents {
                            if comes_after.contains_key(&d) {
                                panic!("{} stopped before its dependent {}", moniker, d);
                            }
                        }
                    }
                    _ => {
                        panic!("Unexpected lifecycle type");
                    }
                }
            }
        }
    }

    /// Shut down `a`:
    ///  a
    ///   \
    ///    b
    ///   / \
    ///  c-->d
    /// In this case D uses a resource exposed by C
    #[fuchsia::test]
    async fn shutdown_with_dependency() {
        let components = vec![
            ("root", ComponentDeclBuilder::new().add_lazy_child("a").build()),
            ("a", ComponentDeclBuilder::new().add_eager_child("b").build()),
            (
                "b",
                ComponentDeclBuilder::new()
                    .add_eager_child("c")
                    .add_eager_child("d")
                    .offer(OfferDecl::Protocol(OfferProtocolDecl {
                        source: OfferSource::Child("c".to_string()),
                        source_name: "serviceC".into(),
                        target_name: "serviceC".into(),
                        target: OfferTarget::Child("d".to_string()),
                        dependency_type: DependencyType::Strong,
                    }))
                    .build(),
            ),
            (
                "c",
                ComponentDeclBuilder::new()
                    .protocol(ProtocolDecl {
                        name: "serviceC".into(),
                        source_path: "/svc/serviceC".parse().unwrap(),
                    })
                    .expose(ExposeDecl::Protocol(ExposeProtocolDecl {
                        source: ExposeSource::Self_,
                        source_name: "serviceC".into(),
                        target_name: "serviceC".into(),
                        target: ExposeTarget::Parent,
                    }))
                    .build(),
            ),
            (
                "d",
                ComponentDeclBuilder::new()
                    .use_(UseDecl::Protocol(UseProtocolDecl {
                        source: UseSource::Parent,
                        source_name: "serviceC".into(),
                        target_path: CapabilityPath::try_from("/svc/serviceC").unwrap(),
                        dependency_type: DependencyType::Strong,
                    }))
                    .build(),
            ),
        ];
        let test = ActionsTest::new("root", components, None).await;
        let component_a = test.look_up(vec!["a:0"].into()).await;
        let component_b = test.look_up(vec!["a:0", "b:0"].into()).await;
        let component_c = test.look_up(vec!["a:0", "b:0", "c:0"].into()).await;
        let component_d = test.look_up(vec!["a:0", "b:0", "d:0"].into()).await;

        // Component startup was eager, so they should all have an `Execution`.
        test.model
            .bind(&component_a.abs_moniker, &BindReason::Eager)
            .await
            .expect("could not bind to a");

        let component_a_info = ComponentInfo::new(component_a).await;
        let component_b_info = ComponentInfo::new(component_b).await;
        let component_c_info = ComponentInfo::new(component_c).await;
        let component_d_info = ComponentInfo::new(component_d).await;

        // Register shutdown action on "a", and wait for it. This should cause all components
        // to shut down, in bottom-up and dependency order.
        ActionSet::register(component_a_info.component.clone(), ShutdownAction::new())
            .await
            .expect("shutdown failed");
        component_a_info.check_is_shut_down(&test.runner).await;
        component_b_info.check_is_shut_down(&test.runner).await;
        component_c_info.check_is_shut_down(&test.runner).await;
        component_d_info.check_is_shut_down(&test.runner).await;

        {
            let events: Vec<_> = test
                .test_hook
                .lifecycle()
                .into_iter()
                .filter(|e| match e {
                    Lifecycle::Stop(_) => true,
                    _ => false,
                })
                .collect();
            let expected: Vec<_> = vec![
                Lifecycle::Stop(vec!["a:0", "b:0", "d:0"].into()),
                Lifecycle::Stop(vec!["a:0", "b:0", "c:0"].into()),
                Lifecycle::Stop(vec!["a:0", "b:0"].into()),
                Lifecycle::Stop(vec!["a:0"].into()),
            ];
            assert_eq!(events, expected);
        }
    }

    /// Shut down `a`:
    ///   a     (a use b)
    ///  / \
    /// b    c
    /// In this case, c shuts down first, then a, then b.
    #[fuchsia::test]
    async fn shutdown_use_from_child() {
        let components = vec![
            ("root", ComponentDeclBuilder::new().add_lazy_child("a").build()),
            (
                "a",
                ComponentDeclBuilder::new()
                    .add_eager_child("b")
                    .add_eager_child("c")
                    .use_(UseDecl::Protocol(UseProtocolDecl {
                        source: UseSource::Child("b".to_string()),
                        source_name: "serviceC".into(),
                        target_path: CapabilityPath::try_from("/svc/serviceC").unwrap(),
                        dependency_type: DependencyType::Strong,
                    }))
                    .build(),
            ),
            (
                "b",
                ComponentDeclBuilder::new()
                    .protocol(ProtocolDecl {
                        name: "serviceC".into(),
                        source_path: "/svc/serviceC".parse().unwrap(),
                    })
                    .expose(ExposeDecl::Protocol(ExposeProtocolDecl {
                        source: ExposeSource::Self_,
                        source_name: "serviceC".into(),
                        target_name: "serviceC".into(),
                        target: ExposeTarget::Parent,
                    }))
                    .build(),
            ),
            ("c", ComponentDeclBuilder::new().build()),
        ];
        let test = ActionsTest::new("root", components, None).await;
        let component_a = test.look_up(vec!["a:0"].into()).await;
        let component_b = test.look_up(vec!["a:0", "b:0"].into()).await;
        let component_c = test.look_up(vec!["a:0", "c:0"].into()).await;

        // Component startup was eager, so they should all have an `Execution`.
        test.model
            .bind(&component_a.abs_moniker, &BindReason::Eager)
            .await
            .expect("could not bind to a");

        let component_a_info = ComponentInfo::new(component_a).await;
        let component_b_info = ComponentInfo::new(component_b).await;
        let component_c_info = ComponentInfo::new(component_c).await;

        // Register shutdown action on "a", and wait for it. This should cause all components
        // to shut down.
        ActionSet::register(component_a_info.component.clone(), ShutdownAction::new())
            .await
            .expect("shutdown failed");
        component_a_info.check_is_shut_down(&test.runner).await;
        component_b_info.check_is_shut_down(&test.runner).await;
        component_c_info.check_is_shut_down(&test.runner).await;

        {
            let events: Vec<_> = test
                .test_hook
                .lifecycle()
                .into_iter()
                .filter(|e| match e {
                    Lifecycle::Stop(_) => true,
                    _ => false,
                })
                .collect();
            let expected: Vec<_> = vec![
                Lifecycle::Stop(vec!["a:0", "c:0"].into()),
                Lifecycle::Stop(vec!["a:0"].into()),
                Lifecycle::Stop(vec!["a:0", "b:0"].into()),
            ];
            assert_eq!(events, expected);
        }
    }

    /// Shut down `a`:
    ///   a     (a use b weak)
    ///  / \
    /// b    c
    /// In this case, b or c shutdown first (arbitrary order), then a.
    #[fuchsia::test]
    async fn shutdown_use_from_child_weak() {
        let components = vec![
            ("root", ComponentDeclBuilder::new().add_lazy_child("a").build()),
            (
                "a",
                ComponentDeclBuilder::new()
                    .add_eager_child("b")
                    .add_eager_child("c")
                    .use_(UseDecl::Protocol(UseProtocolDecl {
                        source: UseSource::Child("b".to_string()),
                        source_name: "serviceC".into(),
                        target_path: CapabilityPath::try_from("/svc/serviceC").unwrap(),
                        dependency_type: DependencyType::Weak,
                    }))
                    .build(),
            ),
            (
                "b",
                ComponentDeclBuilder::new()
                    .protocol(ProtocolDecl {
                        name: "serviceC".into(),
                        source_path: "/svc/serviceC".parse().unwrap(),
                    })
                    .expose(ExposeDecl::Protocol(ExposeProtocolDecl {
                        source: ExposeSource::Self_,
                        source_name: "serviceC".into(),
                        target_name: "serviceC".into(),
                        target: ExposeTarget::Parent,
                    }))
                    .build(),
            ),
            ("c", ComponentDeclBuilder::new().build()),
        ];
        let test = ActionsTest::new("root", components, None).await;
        let component_a = test.look_up(vec!["a:0"].into()).await;
        let component_b = test.look_up(vec!["a:0", "b:0"].into()).await;
        let component_c = test.look_up(vec!["a:0", "c:0"].into()).await;

        // Component startup was eager, so they should all have an `Execution`.
        test.model
            .bind(&component_a.abs_moniker, &BindReason::Eager)
            .await
            .expect("could not bind to a");

        let component_a_info = ComponentInfo::new(component_a).await;
        let component_b_info = ComponentInfo::new(component_b).await;
        let component_c_info = ComponentInfo::new(component_c).await;

        // Register shutdown action on "a", and wait for it. This should cause all components
        // to shut down.
        ActionSet::register(component_a_info.component.clone(), ShutdownAction::new())
            .await
            .expect("shutdown failed");
        component_a_info.check_is_shut_down(&test.runner).await;
        component_b_info.check_is_shut_down(&test.runner).await;
        component_c_info.check_is_shut_down(&test.runner).await;

        {
            let events: Vec<_> = test
                .test_hook
                .lifecycle()
                .into_iter()
                .filter(|e| match e {
                    Lifecycle::Stop(_) => true,
                    _ => false,
                })
                .collect();
            let expected1: Vec<_> = vec![
                Lifecycle::Stop(vec!["a:0", "c:0"].into()),
                Lifecycle::Stop(vec!["a:0", "b:0"].into()),
                Lifecycle::Stop(vec!["a:0"].into()),
            ];
            let expected2: Vec<_> = vec![
                Lifecycle::Stop(vec!["a:0", "b:0"].into()),
                Lifecycle::Stop(vec!["a:0", "c:0"].into()),
                Lifecycle::Stop(vec!["a:0"].into()),
            ];
            assert!(events == expected1 || events == expected2);
        }
    }

    /// Shut down `b`:
    ///  a
    ///   \
    ///    b
    ///     \
    ///      b
    ///       \
    ///      ...
    ///
    /// `b` is a child of itself, but shutdown should still be able to complete.
    #[fuchsia::test]
    async fn shutdown_self_referential() {
        let components = vec![
            ("root", ComponentDeclBuilder::new().add_lazy_child("a").build()),
            ("a", ComponentDeclBuilder::new().add_lazy_child("b").build()),
            ("b", ComponentDeclBuilder::new().add_lazy_child("b").build()),
        ];
        let test = ActionsTest::new("root", components, None).await;
        let component_a = test.look_up(vec!["a:0"].into()).await;
        let component_b = test.look_up(vec!["a:0", "b:0"].into()).await;
        let component_b2 = test.look_up(vec!["a:0", "b:0", "b:0"].into()).await;

        // Bind to second `b`.
        test.model
            .bind(&component_a.abs_moniker, &BindReason::Eager)
            .await
            .expect("could not bind to b2");
        test.model
            .bind(&component_b.abs_moniker, &BindReason::Eager)
            .await
            .expect("could not bind to b2");
        test.model
            .bind(&component_b2.abs_moniker, &BindReason::Eager)
            .await
            .expect("could not bind to b2");
        assert!(is_executing(&component_a).await);
        assert!(is_executing(&component_b).await);
        assert!(is_executing(&component_b2).await);

        let component_a_info = ComponentInfo::new(component_a).await;
        let component_b_info = ComponentInfo::new(component_b).await;
        let component_b2_info = ComponentInfo::new(component_b2).await;

        // Register shutdown action on "a", and wait for it. This should cause all components
        // to shut down, in bottom-up and dependency order.
        ActionSet::register(component_a_info.component.clone(), ShutdownAction::new())
            .await
            .expect("shutdown failed");
        component_a_info.check_is_shut_down(&test.runner).await;
        component_b_info.check_is_shut_down(&test.runner).await;
        component_b2_info.check_is_shut_down(&test.runner).await;
        {
            let events: Vec<_> = test
                .test_hook
                .lifecycle()
                .into_iter()
                .filter(|e| match e {
                    Lifecycle::Stop(_) => true,
                    _ => false,
                })
                .collect();
            assert_eq!(
                events,
                vec![
                    Lifecycle::Stop(vec!["a:0", "b:0", "b:0"].into()),
                    Lifecycle::Stop(vec!["a:0", "b:0"].into()),
                    Lifecycle::Stop(vec!["a:0"].into())
                ]
            );
        }
    }

    /// Shut down `a`:
    ///  a
    ///   \
    ///    b
    ///   / \
    ///  c   d
    ///
    /// `b` fails to finish shutdown the first time, but succeeds the second time.
    #[fuchsia::test]
    async fn shutdown_error() {
        struct StopErrorHook {
            moniker: AbsoluteMoniker,
        }

        impl StopErrorHook {
            fn new(moniker: AbsoluteMoniker) -> Self {
                Self { moniker }
            }

            fn hooks(self: &Arc<Self>) -> Vec<HooksRegistration> {
                vec![HooksRegistration::new(
                    "StopErrorHook",
                    vec![EventType::Stopped],
                    Arc::downgrade(self) as Weak<dyn Hook>,
                )]
            }

            async fn on_shutdown_instance_async(
                &self,
                target_moniker: &AbsoluteMoniker,
            ) -> Result<(), ModelError> {
                if *target_moniker == self.moniker {
                    return Err(ModelError::unsupported("ouch"));
                }
                Ok(())
            }
        }

        #[async_trait]
        impl Hook for StopErrorHook {
            async fn on(self: Arc<Self>, event: &hooks::Event) -> Result<(), ModelError> {
                let target_moniker = event
                    .target_moniker
                    .unwrap_instance_moniker_or(ModelError::UnexpectedComponentManagerMoniker)?;
                if let Ok(EventPayload::Stopped { .. }) = event.result {
                    self.on_shutdown_instance_async(target_moniker).await?;
                }
                Ok(())
            }
        }

        let components = vec![
            ("root", ComponentDeclBuilder::new().add_lazy_child("a").build()),
            ("a", ComponentDeclBuilder::new().add_eager_child("b").build()),
            ("b", ComponentDeclBuilder::new().add_eager_child("c").add_eager_child("d").build()),
            ("c", component_decl_with_test_runner()),
            ("d", component_decl_with_test_runner()),
        ];
        let error_hook = Arc::new(StopErrorHook::new(vec!["a:0", "b:0"].into()));
        let test = ActionsTest::new_with_hooks("root", components, None, error_hook.hooks()).await;
        let component_a = test.look_up(vec!["a:0"].into()).await;
        let component_b = test.look_up(vec!["a:0", "b:0"].into()).await;
        let component_c = test.look_up(vec!["a:0", "b:0", "c:0"].into()).await;
        let component_d = test.look_up(vec!["a:0", "b:0", "d:0"].into()).await;

        // Component startup was eager, so they should all have an `Execution`.
        test.model
            .bind(&component_a.abs_moniker, &BindReason::Eager)
            .await
            .expect("could not bind to a");
        assert!(is_executing(&component_a).await);
        assert!(is_executing(&component_b).await);
        assert!(is_executing(&component_c).await);
        assert!(is_executing(&component_d).await);

        let component_a_info = ComponentInfo::new(component_a).await;
        let component_b_info = ComponentInfo::new(component_b).await;
        let component_c_info = ComponentInfo::new(component_c).await;
        let component_d_info = ComponentInfo::new(component_d).await;

        // Register shutdown action on "a", and wait for it. "b"'s component shuts down, but "b"
        // returns an error so "a" does not.
        ActionSet::register(component_a_info.component.clone(), ShutdownAction::new())
            .await
            .expect_err("shutdown succeeded unexpectedly");
        component_a_info.check_not_shut_down(&test.runner).await;
        component_b_info.check_is_shut_down(&test.runner).await;
        component_c_info.check_is_shut_down(&test.runner).await;
        component_d_info.check_is_shut_down(&test.runner).await;
        {
            let mut events: Vec<_> = test
                .test_hook
                .lifecycle()
                .into_iter()
                .filter(|e| match e {
                    Lifecycle::Stop(_) => true,
                    _ => false,
                })
                .collect();
            // The leaves could be stopped in any order.
            let mut first: Vec<_> = events.drain(0..2).collect();
            first.sort_unstable();
            let expected: Vec<_> = vec![
                Lifecycle::Stop(vec!["a:0", "b:0", "c:0"].into()),
                Lifecycle::Stop(vec!["a:0", "b:0", "d:0"].into()),
            ];
            assert_eq!(first, expected);
            assert_eq!(events, vec![Lifecycle::Stop(vec!["a:0", "b:0"].into())],);
        }

        // Register shutdown action on "a" again. "b"'s shutdown succeeds (it's a no-op), and
        // "a" is allowed to shut down this time.
        ActionSet::register(component_a_info.component.clone(), ShutdownAction::new())
            .await
            .expect("shutdown failed");
        component_a_info.check_is_shut_down(&test.runner).await;
        component_b_info.check_is_shut_down(&test.runner).await;
        component_c_info.check_is_shut_down(&test.runner).await;
        component_d_info.check_is_shut_down(&test.runner).await;
        {
            let mut events: Vec<_> = test
                .test_hook
                .lifecycle()
                .into_iter()
                .filter(|e| match e {
                    Lifecycle::Stop(_) => true,
                    _ => false,
                })
                .collect();
            // The leaves could be stopped in any order.
            let mut first: Vec<_> = events.drain(0..2).collect();
            first.sort_unstable();
            let expected: Vec<_> = vec![
                Lifecycle::Stop(vec!["a:0", "b:0", "c:0"].into()),
                Lifecycle::Stop(vec!["a:0", "b:0", "d:0"].into()),
            ];
            assert_eq!(first, expected);
            assert_eq!(
                events,
                vec![
                    Lifecycle::Stop(vec!["a:0", "b:0"].into()),
                    Lifecycle::Stop(vec!["a:0"].into())
                ]
            );
        }
    }
}
