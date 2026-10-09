// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::ptr::NonNull;
use std::rc::Weak;

use accesskit::{
    Action, ActionRequest, Affine, Live, Node, NodeId, Orientation, Role, Toggled, Tree, TreeId,
    TreeUpdate,
};
use i_slint_core::SharedString;
use i_slint_core::accessibility::{
    AccessibilityAction, AccessibleStringProperty, SupportedAccessibilityAction,
    find_exposed_text_input, nearest_accessible_item,
};
use i_slint_core::api::Window;
use i_slint_core::input::FocusReason;
use i_slint_core::item_tree::{ItemTreeRc, ItemTreeRef, ItemTreeWeak};
use i_slint_core::items::{ItemRc, WindowItem};
use i_slint_core::lengths::{LogicalPoint, ScaleFactor};
use i_slint_core::window::{PopupWindowLocation, WindowInner};
use i_slint_core::{properties::PropertyTracker, window::WindowAdapter};

use super::WinitWindowAdapter;
use crate::SlintEvent;
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};

/// The AccessKit adapter tries to keep the given window adapter's item tree in sync with accesskit's node tree.
///
/// The entire item tree is mapped to accesskit's node tree. Any changes to an individual accessible item results
/// in an access kit tree update with just changed nodes. Any changes in the tree structure result in a complete
/// tree rebuild. This could be implemented more efficiently, but that isn't essential; AccessKit will avoid firing
/// gratuitous events for full-tree updates as long as the node IDs are stable.
///
/// For unix it's necessary to inform accesskit about any changes to the position or size of the window, hence
/// the `on_event` function that needs calling.
///
/// Similarly, when the window adapter is informed about a focus change, handle_focus_change must be called.
/// Finally, when a component is destroyed, `unregister_item_tree` must be called, which rebuilds the entire
/// tree at the moment.
///
/// If we wanted to move this to corelib, `on_event` gets replaced with listening to the events sent from the
/// platform adapter to the slint::Window. `handle_focus_change` is already internal to WindowInner, as well
/// as `component_destroyed`. The `WindowInner` would own this `AccessKit`.
pub struct AccessKitAdapter {
    inner: accesskit_winit::Adapter,
    window_adapter_weak: Weak<WinitWindowAdapter>,
    nodes: NodeCollection,
    global_property_tracker: Pin<Box<AccessibilityPropertyTracker>>,
    pending_update: bool,
    initial_tree_sent: bool,
}

impl AccessKitAdapter {
    pub fn new(
        window_adapter_weak: Weak<WinitWindowAdapter>,
        active_event_loop: &ActiveEventLoop,
        winit_window: &winit::window::Window,
        proxy: EventLoopProxy<SlintEvent>,
    ) -> Self {
        Self {
            inner: accesskit_winit::Adapter::with_event_loop_proxy(
                active_event_loop,
                winit_window,
                proxy,
            ),
            window_adapter_weak: window_adapter_weak.clone(),
            nodes: NodeCollection {
                next_component_id: 1,
                free_component_ids: Default::default(),
                root_node_id: NodeId(0),
                components_by_id: Default::default(),
                component_ids: Default::default(),
                all_nodes: Default::default(),
                focused_node_tracker: Box::pin(PropertyTracker::new_with_dirty_handler(
                    DelegateFocusPropertyTracker {
                        window_adapter_weak: window_adapter_weak.clone(),
                    },
                )),
                text_state: Default::default(),
            },
            global_property_tracker: Box::pin(PropertyTracker::new_with_dirty_handler(
                AccessibilityPropertyDirtyHandler {
                    window_adapter_weak: window_adapter_weak.clone(),
                },
            )),
            pending_update: false,
            initial_tree_sent: false,
        }
    }

    pub fn process_event(
        &mut self,
        window: &winit::window::Window,
        event: &winit::event::WindowEvent,
    ) {
        if matches!(event, winit::event::WindowEvent::Focused(_)) {
            self.global_property_tracker.set_dirty();
            self.invoke_later(|self_cell, _| self_cell.borrow_mut().rebuild_tree_of_dirty_nodes());
        }
        self.inner.process_event(window, event);
    }

    pub fn process_accesskit_event(
        &mut self,
        window_event: accesskit_winit::WindowEvent,
    ) -> Option<DeferredAccessKitAction> {
        match window_event {
            accesskit_winit::WindowEvent::InitialTreeRequested => {
                self.inner.update_if_active(|| {
                    self.nodes.build_new_tree(
                        &self.window_adapter_weak,
                        self.global_property_tracker.as_ref(),
                        true,
                    )
                });
                self.initial_tree_sent = true;
                None
            }
            accesskit_winit::WindowEvent::ActionRequested(r) => self.handle_request(r),
            accesskit_winit::WindowEvent::AccessibilityDeactivated => {
                self.initial_tree_sent = false;
                self.nodes.text_state.clear_all();
                None
            }
        }
    }

    pub fn handle_focus_item_change(&mut self) {
        // Ignore focus changes until an initial tree was sent, to avoid sending `tree: None`.
        if !self.initial_tree_sent {
            return;
        }
        // Don't send a tree update now with an empty tree/node list when we know that the structure
        // if the tree has changed. It might be that the focus node is not known yet to AccessKit.
        // The pending update will take care of setting the focus node.
        if self.pending_update {
            return;
        }
        self.inner.update_if_active(|| TreeUpdate {
            nodes: Vec::new(),
            tree: None,
            tree_id: TreeId::ROOT,
            focus: self.nodes.focus_node(&self.window_adapter_weak),
        })
    }

    fn handle_request(&self, request: ActionRequest) -> Option<DeferredAccessKitAction> {
        let a = match request.action {
            Action::Click => AccessibilityAction::Default,
            Action::Focus => {
                return self
                    .nodes
                    .item_rc_for_node_id(request.target_node)
                    .map(DeferredAccessKitAction::SetFocus);
            }
            Action::Decrement => AccessibilityAction::Decrement,
            Action::Increment => AccessibilityAction::Increment,
            Action::ReplaceSelectedText => {
                let Some(accesskit::ActionData::Value(v)) = request.data else { return None };
                AccessibilityAction::ReplaceSelectedText(SharedString::from(&*v))
            }
            Action::SetValue => match request.data {
                Some(accesskit::ActionData::Value(v)) => {
                    AccessibilityAction::SetValue(SharedString::from(&*v))
                }
                Some(accesskit::ActionData::NumericValue(v)) => {
                    AccessibilityAction::SetValue(i_slint_core::format!("{v}"))
                }
                _ => return None,
            },
            Action::Expand => AccessibilityAction::Expand,
            Action::SetTextSelection => {
                let Some(accesskit::ActionData::SetTextSelection(sel)) = request.data.as_ref()
                else {
                    return None;
                };
                // A TextPosition names a TextRun sub-NodeId, so decode it back to the wrapper.
                let (wrapper_parent, _) = decode_sub_node_id(sel.focus.node)?;
                if wrapper_parent != request.target_node {
                    return None;
                }
                let wrapper_item = self.nodes.item_rc_for_node_id(wrapper_parent)?;
                let window_adapter = self.window_adapter_weak.upgrade()?;
                let (inner_item_rc, text_input) = find_exposed_text_input(&wrapper_item)?;
                let state = self
                    .nodes
                    .text_state
                    .get_or_update_cache_entry_ref(&inner_item_rc, Default::default);
                let (anchor, focus) = state.decode_selection(
                    window_adapter.renderer().as_core_renderer(),
                    text_input.as_pin_ref(),
                    &inner_item_rc,
                    inner_item_rc.geometry().size,
                    &sel.anchor,
                    &sel.focus,
                )?;
                AccessibilityAction::SetSelectionOffsets(anchor as i32, focus as i32)
            }
            _ => return None,
        };
        self.nodes
            .item_rc_for_node_id(request.target_node)
            .map(|item| DeferredAccessKitAction::InvokeAccessibleAction(item, a))
    }

    pub fn reload_tree(&mut self) {
        if self.pending_update {
            return;
        }
        self.pending_update = true;

        self.invoke_later(|self_cell, win| {
            let mut self_ = self_cell.borrow_mut();
            let self_ = &mut *self_;
            self_.pending_update = false;
            self_.inner.update_if_active(|| {
                self_.nodes.build_new_tree(&win, self_.global_property_tracker.as_ref(), false)
            })
        });
    }

    pub fn unregister_item_tree(&mut self, component: ItemTreeRef) {
        self.nodes.forget_component(ItemTreeRef::as_ptr(component));
        self.reload_tree();
    }

    fn rebuild_tree_of_dirty_nodes(&mut self) {
        if !self.global_property_tracker.is_dirty() && !self.nodes.focused_node_tracker.is_dirty() {
            return;
        }

        // It's possible that we may have been triggered by a timer, but in the meantime
        // the node tree has been emptied due to a tree structure change.
        if self.nodes.all_nodes.is_empty() {
            return;
        }

        // Don't end a tree update now with an empty tree/node list when we know that the structure
        // if the tree has changed. It might be that the focus node is not known yet to AccessKit.
        // The pending update will take care rebuilding the entire tree anyway.
        if self.pending_update {
            return;
        }

        let Some(window_adapter) = self.window_adapter_weak.upgrade() else { return };
        let scale_factor = ScaleFactor::new(window_adapter.window().scale_factor());

        self.inner.update_if_active(|| {
            self.global_property_tracker.as_ref().evaluate_as_dependency_root(|| {
                let t0 = std::time::Instant::now();
                let mut cached = std::mem::take(&mut self.nodes.all_nodes);
                let mut nodes_vec: Vec<(NodeId, Node)> = Vec::new();
                for c in cached.iter_mut() {
                    self.nodes.refresh_cached_node(c, scale_factor, &window_adapter, &mut nodes_vec);
                }
                stat("dirty", nodes_vec.len(), cached.len(), t0.elapsed());
                self.nodes.all_nodes = cached;

                TreeUpdate {
                    nodes: nodes_vec,
                    tree: None,
                    tree_id: TreeId::ROOT,
                    focus: self.nodes.focus_node(&self.window_adapter_weak),
                }
            })
        })
    }

    fn invoke_later(
        &self,
        callback: impl FnOnce(&std::cell::RefCell<Self>, Weak<WinitWindowAdapter>) + 'static,
    ) {
        let win = self.window_adapter_weak.clone();
        i_slint_core::timers::Timer::single_shot(accessibility_update_delay(), move || {
            WinitWindowAdapter::with_access_kit_adapter_from_weak_window_adapter(
                win.clone(),
                move |self_cell| callback(self_cell, win),
            );
        });
    }
}

/// Accessibility tree updates are coalesced behind this delay instead of running on the next
/// event-loop turn. During a 60 Hz scroll every frame destroys rows that left the viewport
/// (full `reload_tree`) and moves every visible row (dirty-node rebuild); with a zero delay
/// both ran, and raised UIA property-changed events, on every frame. Assistive clients do
/// not consume updates at frame rate, and `reload_tree` already dedups through
/// `pending_update`, so a short delay bounds the work to at most ~20 updates per second.
/// Override with `SLINT_ACCESSKIT_UPDATE_DELAY_MS` for experiments (0 restores the old behavior).
fn accessibility_update_delay() -> core::time::Duration {
    static DELAY_MS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let ms = *DELAY_MS.get_or_init(|| {
        std::env::var("SLINT_ACCESSKIT_UPDATE_DELAY_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(50)
    });
    core::time::Duration::from_millis(ms)
}

const NODE_ID_INDEX_BITS: u32 = 16;
const NODE_ID_INDEX_MASK: u64 = (1 << NODE_ID_INDEX_BITS) - 1; // 0xFFFF
const NODE_ID_COMPONENT_BITS: u32 = 22;
const NODE_ID_COMPONENT_MASK: u64 = (1 << NODE_ID_COMPONENT_BITS) - 1; // 0x3FFFFF

// NodeIds for the TextRun children of a text input:
//
//   bits 38..=63 : sub-index, allocated per parent, never 0
//   bits 0..=37  : the parent NodeId, which `encode_item_node_id` fits in exactly these bits
//
// A regular NodeId leaves the sub-index bits clear, so a non-zero sub-index is what tells the two
// apart.
const NODE_ID_PARENT_BITS: u32 = NODE_ID_INDEX_BITS + NODE_ID_COMPONENT_BITS;
const NODE_ID_PARENT_MASK: u64 = (1u64 << NODE_ID_PARENT_BITS) - 1;
const NODE_ID_SUB_INDEX_BITS: u32 = u64::BITS - NODE_ID_PARENT_BITS;
const NODE_ID_SUB_INDEX_MASK: u64 = (1u64 << NODE_ID_SUB_INDEX_BITS) - 1;

fn encode_sub_node_id(parent: NodeId, sub_index: u32) -> NodeId {
    debug_assert!(sub_index >= 1, "sub_index 0 would collide with the parent NodeId");
    debug_assert!(
        (sub_index as u64) <= NODE_ID_SUB_INDEX_MASK,
        "sub_index exceeds {NODE_ID_SUB_INDEX_BITS} bits"
    );
    debug_assert_eq!(
        parent.0 & !NODE_ID_PARENT_MASK,
        0,
        "parent NodeId occupies more than {NODE_ID_PARENT_BITS} bits"
    );
    NodeId(((sub_index as u64) << NODE_ID_PARENT_BITS) | (parent.0 & NODE_ID_PARENT_MASK))
}

fn decode_sub_node_id(id: NodeId) -> Option<(NodeId, u32)> {
    let sub_index = (id.0 >> NODE_ID_PARENT_BITS) as u32;
    (sub_index != 0).then_some((NodeId(id.0 & NODE_ID_PARENT_MASK), sub_index))
}

fn is_text_input_role(role: Role) -> bool {
    matches!(
        role,
        Role::TextInput
            | Role::MultilineTextInput
            | Role::PasswordInput
            | Role::SearchInput
            | Role::NumberInput
    )
}

fn wraps_text_input(role: Role) -> bool {
    is_text_input_role(role) || role == Role::SpinButton
}

struct NodeCollection {
    next_component_id: u32,
    free_component_ids: VecDeque<u32>,
    components_by_id: HashMap<u32, ItemTreeWeak>,
    component_ids: HashMap<NonNull<u8>, u32>,
    all_nodes: Vec<CachedNode>,
    root_node_id: NodeId,
    focused_node_tracker: Pin<Box<PropertyTracker<false, DelegateFocusPropertyTracker>>>,
    /// Emission state, keyed by the inner `TextInput`'s `ItemRc`. Its per-entry property tracker
    /// stays empty on purpose: the state has to outlive the edits it describes, so that NodeIds
    /// stay stable and screen readers see "node updated" rather than "subtree replaced".
    text_state: i_slint_core::item_rendering::ItemCache<
        i_slint_core::textlayout::sharedparley::CachedTextInputAccessibilityState,
    >,
}

impl NodeCollection {
    fn focus_node(&mut self, window_adapter_weak: &Weak<WinitWindowAdapter>) -> NodeId {
        window_adapter_weak
            .upgrade()
            .filter(|window_adapter| {
                window_adapter.winit_window().is_some_and(|winit_window| winit_window.has_focus())
            })
            .and_then(|window_adapter| {
                let window_inner = WindowInner::from_pub(window_adapter.window());
                window_inner
                    .focus_item
                    .borrow()
                    .upgrade()
                    .map(|focus_item| {
                        let parent = nearest_accessible_item(focus_item);
                        self.focused_node_tracker
                            .as_ref()
                            .evaluate(|| {
                                parent.accessible_string_property(
                                    AccessibleStringProperty::DelegateFocus,
                                )
                            })
                            .and_then(|s| s.parse::<usize>().ok())
                            .and_then(|i| {
                                i_slint_core::accessibility::accessible_descendents(&parent).nth(i)
                            })
                            .unwrap_or(parent)
                    })
                    .or_else(|| window_inner.try_component().map(ItemRc::new_root))
                    .map(|focus_item| self.find_node_id_by_item_rc(focus_item))
            })
            .unwrap_or(self.root_node_id)
    }

    fn item_rc_for_node_id(&self, id: NodeId) -> Option<ItemRc> {
        let component_id: u32 = ((id.0 >> NODE_ID_INDEX_BITS) & NODE_ID_COMPONENT_MASK) as _;
        let index: u32 = (id.0 & NODE_ID_INDEX_MASK) as _;
        let component = self.components_by_id.get(&component_id)?.upgrade()?;
        Some(ItemRc::new(component, index))
    }

    fn find_node_id_by_item_rc(&mut self, mut item: ItemRc) -> NodeId {
        item = nearest_accessible_item(item);

        self.encode_item_node_id(&item)
    }

    fn alloc_component_id(&mut self) -> u32 {
        // Reusing an id right after it was freed makes AccessKit see a component created in
        // this frame as an update of the one destroyed in the same frame (new name, new
        // position in set), which raises a property-changed event per node: a ListView
        // scroll produced dozens per frame. Keep freed ids parked until plenty of others
        // have been freed, so a reused id has long been gone from the tree.
        const PARKED_IDS: usize = 1024;
        if self.free_component_ids.len() > PARKED_IDS {
            return self.free_component_ids.pop_front().unwrap();
        }
        let id = self.next_component_id;
        self.next_component_id += 1;
        id
    }

    fn forget_component(&mut self, component_ptr: NonNull<u8>) {
        if let Some(component_id) = self.component_ids.remove(&component_ptr) {
            self.components_by_id.remove(&component_id);
            self.free_component_ids.push_back(component_id);
        }
        self.text_state.component_destroyed_at(component_ptr);
    }

    /// Forgets the components that were destroyed without being unregistered: their
    /// `unregister_item_tree` is skipped when it finds the adapter borrowed, such as while
    /// building the tree destroys repeated components (#13670).
    fn forget_destroyed_components(&mut self) {
        let destroyed = self
            .component_ids
            .iter()
            .filter(|(_, id)| self.components_by_id.get(id).is_none_or(|c| c.upgrade().is_none()))
            .map(|(component_ptr, _)| *component_ptr)
            .collect::<Vec<_>>();
        for component_ptr in destroyed {
            self.forget_component(component_ptr);
        }
    }

    fn encode_item_node_id(&mut self, item: &ItemRc) -> NodeId {
        let component = item.item_tree();
        let component_ptr = ItemTreeRef::as_ptr(ItemTreeRc::borrow(component));
        let component_id = match self.component_ids.get(&component_ptr) {
            Some(&component_id) => component_id,
            None => {
                let component_id = self.alloc_component_id();
                self.component_ids.insert(component_ptr, component_id);
                self.components_by_id.insert(component_id, ItemTreeRc::downgrade(component));
                component_id
            }
        };

        debug_assert!(
            (component_id as u64) <= NODE_ID_COMPONENT_MASK,
            "component_id exceeds {NODE_ID_COMPONENT_BITS} bits"
        );
        let index = item.index();
        NodeId((component_id as u64) << NODE_ID_INDEX_BITS | (index as u64 & NODE_ID_INDEX_MASK))
    }

    fn build_node_for_item_recursively(
        &mut self,
        item: ItemRc,
        nodes: &mut Vec<(NodeId, Node)>,
        popups: &[AccessiblePopup],
        scale_factor: ScaleFactor,
        window_position: LogicalPoint,
        window_adapter: &std::rc::Rc<WinitWindowAdapter>,
        base: Option<NodeId>,
        full: bool,
        old: &mut HashMap<NodeId, CachedNode>,
    ) -> NodeId {
        let id = self.encode_item_node_id(&item);

        // Popups are child windows at their own window position: never inside a content node.
        let popup_children = popups
            .iter()
            .filter_map(|popup| {
                if popup.parent_node != id {
                    return None;
                }

                let popup_item = ItemRc::new_root(popup.component.clone());
                Some(self.build_node_for_item_recursively(
                    popup_item,
                    nodes,
                    popups,
                    scale_factor,
                    popup.location,
                    window_adapter,
                    None,
                    full,
                    old,
                ))
            })
            .collect::<Vec<_>>();

        // Accessible children that sit inside a Flickable (the ScrollView's Flickable is not
        // itself accessible; the item with the list role is above it) go under a synthetic
        // content node that carries the scroll offset as a transform. Their bounds are then
        // relative to that node, so a scroll changes one node per Flickable instead of every
        // visible row. Children are grouped by Flickable, in order of first appearance.
        let mut groups: Vec<(Option<NodeId>, Vec<NodeId>)> = Vec::new();
        for child in i_slint_core::accessibility::accessible_descendents(&item) {
            let content_id = nearest_flickable_below(&child, &item)
                .map(|flickable| encode_sub_node_id(self.encode_item_node_id(&flickable), FLICKABLE_CONTENT_SUB_INDEX));
            let child_base = content_id.or(base);
            let child_id = self.build_node_for_item_recursively(
                child,
                nodes,
                popups,
                scale_factor,
                window_position,
                window_adapter,
                child_base,
                full,
                old,
            );
            match groups.iter_mut().find(|(c, _)| *c == content_id) {
                Some((_, ids)) => ids.push(child_id),
                None => groups.push((content_id, vec![child_id])),
            }
        }

        let mut own_children = Vec::new();
        for (content_id, ids) in groups {
            match content_id {
                Some(content_id) => {
                    self.build_cached_node(
                        content_id,
                        CachedNodeKind::FlickableContent,
                        base,
                        ids,
                        window_position,
                        scale_factor,
                        window_adapter,
                        full,
                        old,
                        nodes,
                    );
                    own_children.push(content_id);
                }
                None => own_children.extend(ids),
            }
        }
        own_children.extend(popup_children);

        self.build_cached_node(
            id,
            CachedNodeKind::Item,
            base,
            own_children,
            window_position,
            scale_factor,
            window_adapter,
            full,
            old,
            nodes,
        );

        id
    }

    /// Reuses the cache entry for `id` when its place in the tree is unchanged (only what is
    /// dirty is rebuilt, and nothing is sent when the result is identical), otherwise builds a
    /// new one. Either way the entry ends up in `all_nodes`.
    fn build_cached_node(
        &mut self,
        id: NodeId,
        kind: CachedNodeKind,
        base: Option<NodeId>,
        children: Vec<NodeId>,
        window_position: LogicalPoint,
        scale_factor: ScaleFactor,
        window_adapter: &std::rc::Rc<WinitWindowAdapter>,
        full: bool,
        old: &mut HashMap<NodeId, CachedNode>,
        nodes: &mut Vec<(NodeId, Node)>,
    ) {
        let reusable = if full { None } else { old.remove(&id) };
        let mut cached = match reusable {
            Some(c) if c.children == children && c.base == base => c,
            _ => CachedNode {
                id,
                kind,
                base,
                children,
                window_position,
                tracker: Box::pin(PropertyTracker::default()),
                geometry_tracker: Box::pin(PropertyTracker::default()),
                last: None,
            },
        };
        self.refresh_cached_node(&mut cached, scale_factor, window_adapter, nodes);
        self.all_nodes.push(cached);
    }

    /// Brings a cache entry up to date and pushes the node to `nodes` if AccessKit has not
    /// seen this exact node yet. A clean entry costs nothing; a geometry-only change costs a
    /// geometry read and a compare, and is only sent when the bounds really moved in the
    /// entry's coordinate space.
    fn refresh_cached_node(
        &self,
        cached: &mut CachedNode,
        scale_factor: ScaleFactor,
        window_adapter: &std::rc::Rc<WinitWindowAdapter>,
        nodes: &mut Vec<(NodeId, Node)>,
    ) {
        let base_item = cached
            .base
            .and_then(decode_sub_node_id)
            .and_then(|(flickable_id, _)| self.item_rc_for_node_id(flickable_id));
        let window_position = cached.window_position;
        let id = cached.id;

        match cached.kind {
            CachedNodeKind::FlickableContent => {
                let Some((flickable_id, _)) = decode_sub_node_id(id) else { return };
                let Some(flickable) = self.item_rc_for_node_id(flickable_id) else { return };
                if cached.last.is_some() && !cached.geometry_tracker.is_dirty() {
                    return;
                }
                let transform = cached.geometry_tracker.as_ref().evaluate(|| {
                    self.flickable_content_transform(
                        &flickable,
                        base_item.as_ref(),
                        scale_factor,
                        window_position,
                    )
                });
                if cached.last.as_ref().and_then(|n| n.transform()) == Some(&transform) {
                    return;
                }
                let mut node = Node::new(Role::GenericContainer);
                node.set_transform(transform);
                node.set_children(cached.children.clone());
                nodes.push((id, node.clone()));
                cached.last = Some(node);
            }
            CachedNodeKind::Item => {
                let Some(item) = self.item_rc_for_node_id(id) else { return };
                if cached.last.is_none() || cached.tracker.is_dirty() {
                    let mut text_run_nodes: Vec<(NodeId, Node)> = Vec::new();
                    // One tracker for both the wrapper attributes and the text emission, so
                    // that either going dirty rebuilds the node.
                    let mut node = cached.tracker.as_ref().evaluate(|| {
                        let mut n =
                            self.build_node_without_children(&item, scale_factor, window_position);
                        n.set_children(cached.children.clone());
                        self.try_emit_text_input_accessibility(
                            &item,
                            &mut n,
                            id,
                            scale_factor,
                            window_position,
                            &mut text_run_nodes,
                            window_adapter,
                            base_item.as_ref(),
                        );
                        n
                    });
                    let (bounds, hidden) = cached.geometry_tracker.as_ref().evaluate(|| {
                        self.item_geometry(&item, base_item.as_ref(), scale_factor, window_position)
                    });
                    node.set_bounds(bounds);
                    if hidden {
                        node.set_hidden();
                    }
                    if cached.last.as_ref() != Some(&node) || !text_run_nodes.is_empty() {
                        nodes.push((id, node.clone()));
                        nodes.extend(text_run_nodes);
                    }
                    cached.last = Some(node);
                } else if cached.geometry_tracker.is_dirty() {
                    let (bounds, hidden) = cached.geometry_tracker.as_ref().evaluate(|| {
                        self.item_geometry(&item, base_item.as_ref(), scale_factor, window_position)
                    });
                    let last = cached.last.as_mut().expect("checked above");
                    if last.bounds() != Some(bounds) || last.is_hidden() != hidden {
                        last.set_bounds(bounds);
                        if hidden {
                            last.set_hidden();
                        } else {
                            last.clear_hidden();
                        }
                        nodes.push((id, last.clone()));
                    }
                }
            }
        }
    }

    /// `p` (in `item`'s coordinates) in the coordinate space its node is expressed in: the
    /// content space of the nearest Flickable content node, or the window.
    fn origin_in_space(
        &self,
        item: &ItemRc,
        p: LogicalPoint,
        base: Option<&ItemRc>,
        window_position: LogicalPoint,
    ) -> LogicalPoint {
        match base {
            Some(flickable) => {
                let (cx, cy) = flickable_content_offset(flickable);
                let in_flickable = item.map_to_ancestor(p, flickable);
                LogicalPoint::new(in_flickable.x - cx, in_flickable.y - cy)
            }
            None => item.map_to_window(p) + window_position.to_vector(),
        }
    }

    /// Bounds of `item` in its node's coordinate space, and whether it is hidden. Reads only
    /// geometry and visibility, so the geometry tracker stays independent of the content one.
    fn item_geometry(
        &self,
        item: &ItemRc,
        base: Option<&ItemRc>,
        scale_factor: ScaleFactor,
        window_position: LogicalPoint,
    ) -> (accesskit::Rect, bool) {
        let geometry = item.geometry();
        let origin = self.origin_in_space(item, geometry.origin, base, window_position);
        let physical_origin = (origin * scale_factor).cast::<f64>();
        let physical_size = (geometry.size * scale_factor).cast::<f64>();
        (
            accesskit::Rect {
                x0: physical_origin.x,
                y0: physical_origin.y,
                x1: physical_origin.x + physical_size.width,
                y1: physical_origin.y + physical_size.height,
            },
            !item.is_visible(),
        )
    }

    /// The transform of a Flickable's content node: the content origin (the Flickable's origin
    /// plus its scroll offset), in the coordinate space the Flickable's own node uses.
    fn flickable_content_transform(
        &self,
        flickable: &ItemRc,
        base: Option<&ItemRc>,
        scale_factor: ScaleFactor,
        window_position: LogicalPoint,
    ) -> Affine {
        let origin =
            self.origin_in_space(flickable, LogicalPoint::default(), base, window_position);
        let (cx, cy) = flickable_content_offset(flickable);
        let physical = (LogicalPoint::new(origin.x + cx, origin.y + cy) * scale_factor).cast::<f64>();
        Affine::translate((physical.x, physical.y))
    }

    /// Emits the TextRun children of a text input, and the value and selection on `wrapper_node`.
    fn try_emit_text_input_accessibility(
        &self,
        item: &ItemRc,
        wrapper_node: &mut Node,
        wrapper_id: NodeId,
        scale_factor: ScaleFactor,
        window_position: LogicalPoint,
        text_run_nodes: &mut Vec<(NodeId, Node)>,
        window_adapter: &std::rc::Rc<WinitWindowAdapter>,
        base: Option<&ItemRc>,
    ) {
        if !wraps_text_input(wrapper_node.role()) {
            return;
        }
        let Some((inner_item_rc, text_input)) = find_exposed_text_input(item) else {
            return;
        };
        let mut state =
            self.text_state.get_or_update_cache_entry_ref(&inner_item_rc, Default::default);

        // The inner `TextInput`'s geometry: a `LineEdit` insets it for its border, so measuring
        // from the wrapper's origin would place every TextRun off by the padding.
        let inner_geometry = inner_item_rc.geometry();
        let inner_absolute_origin =
            self.origin_in_space(&inner_item_rc, inner_geometry.origin, base, window_position);
        let physical_origin = (inner_absolute_origin * scale_factor).cast::<f64>();

        let mut update =
            TreeUpdate { nodes: Vec::new(), tree: None, tree_id: TreeId::ROOT, focus: NodeId(0) };

        // Borrows the font context itself, so we must not be holding it here.
        let emitted_runs = state.emit(
            window_adapter.renderer().as_core_renderer(),
            text_input.as_pin_ref(),
            &inner_item_rc,
            inner_geometry.size,
            &mut update,
            wrapper_node,
            wrapper_id,
            (physical_origin.x, physical_origin.y),
            encode_sub_node_id,
        );

        if emitted_runs
            && item
                .supported_accessibility_actions()
                .contains(SupportedAccessibilityAction::SetSelectionOffsets)
        {
            wrapper_node.add_action(Action::SetTextSelection);
        }

        text_run_nodes.extend(update.nodes);
    }

    fn tree_info(&self, root: NodeId) -> Tree {
        let mut tree = Tree::new(root);
        tree.toolkit_name = Some("Slint".into());
        tree.toolkit_version = Some(env!("CARGO_PKG_VERSION").into());
        tree
    }

    /// `full` rebuilds and sends every node (a client just attached); otherwise only nodes
    /// whose place in the tree, content or geometry changed are rebuilt and sent.
    fn build_new_tree(
        &mut self,
        window_adapter_weak: &Weak<WinitWindowAdapter>,
        property_tracker: Pin<&AccessibilityPropertyTracker>,
        full: bool,
    ) -> TreeUpdate {
        let Some(window_adapter) = window_adapter_weak.upgrade() else {
            return TreeUpdate {
                nodes: Default::default(),
                tree: Default::default(),
                tree_id: TreeId::ROOT,
                focus: self.root_node_id,
            };
        };
        let window = window_adapter.window();
        let window_inner = i_slint_core::window::WindowInner::from_pub(window);
        window_inner.ensure_tree_instantiated();

        let Some(component) = window_inner.try_component() else {
            return TreeUpdate {
                nodes: Default::default(),
                tree: Default::default(),
                tree_id: TreeId::ROOT,
                focus: self.root_node_id,
            };
        };
        let root_item = ItemRc::new_root(component);

        let popups = window_inner
            .active_popups()
            .iter()
            .filter_map(|popup| {
                let PopupWindowLocation::ChildWindow(location) = popup.location else {
                    return None;
                };

                let parent_item = nearest_accessible_item(popup.parent_item.upgrade()?);
                let parent_node = self.encode_item_node_id(if parent_item.is_accessible() {
                    &parent_item
                } else {
                    &root_item
                });

                Some(AccessiblePopup { location, parent_node, component: popup.component.clone() })
            })
            .collect::<Vec<_>>();

        let t0 = std::time::Instant::now();
        let mut old: HashMap<NodeId, CachedNode> = if full {
            self.all_nodes.clear();
            HashMap::new()
        } else {
            std::mem::take(&mut self.all_nodes).into_iter().map(|c| (c.id, c)).collect()
        };
        let mut nodes = Vec::new();

        let root_id = property_tracker.evaluate_as_dependency_root(|| {
            self.build_node_for_item_recursively(
                root_item,
                &mut nodes,
                &popups,
                ScaleFactor::new(window.scale_factor()),
                Default::default(),
                &window_adapter,
                None,
                full,
                &mut old,
            )
        });
        self.root_node_id = root_id;
        self.forget_destroyed_components();
        stat(if full { "full" } else { "inc" }, nodes.len(), self.all_nodes.len(), t0.elapsed());

        TreeUpdate {
            nodes,
            tree: Some(self.tree_info(root_id)),
            tree_id: TreeId::ROOT,
            focus: self.focus_node(window_adapter_weak),
        }
    }

    fn build_node_without_children(
        &self,
        item: &ItemRc,
        _scale_factor: ScaleFactor,
        _window_position: LogicalPoint,
    ) -> Node {
        let is_checkable = item
            .accessible_string_property(AccessibleStringProperty::Checkable)
            .is_some_and(|x| x == "true");

        let (role, label) = if let Some(window_item) = item.downcast::<WindowItem>() {
            (Role::Window, Some(window_item.as_pin_ref().title().to_string()))
        } else {
            (
                match item.accessible_role() {
                    i_slint_core::items::AccessibleRole::None => Role::Unknown,
                    i_slint_core::items::AccessibleRole::Button => Role::Button,
                    i_slint_core::items::AccessibleRole::Checkbox => Role::CheckBox,
                    i_slint_core::items::AccessibleRole::Combobox => Role::ComboBox,
                    i_slint_core::items::AccessibleRole::Groupbox => Role::Group,
                    i_slint_core::items::AccessibleRole::List => Role::ListBox,
                    i_slint_core::items::AccessibleRole::Slider => Role::Slider,
                    i_slint_core::items::AccessibleRole::Spinbox => Role::SpinButton,
                    i_slint_core::items::AccessibleRole::Tab => Role::Tab,
                    i_slint_core::items::AccessibleRole::TabList => Role::TabList,
                    i_slint_core::items::AccessibleRole::TabPanel => Role::TabPanel,
                    i_slint_core::items::AccessibleRole::Text => Role::Label,
                    i_slint_core::items::AccessibleRole::Table => Role::Table,
                    i_slint_core::items::AccessibleRole::Tree => Role::Tree,
                    i_slint_core::items::AccessibleRole::TextInput => {
                        if let Some(text_input) = i_slint_core::accessibility::find_text_input(item)
                        {
                            if !text_input.single_line.get_internal() {
                                Role::MultilineTextInput
                            } else {
                                match text_input.input_type.get_internal() {
                                    i_slint_core::items::InputType::Decimal
                                    | i_slint_core::items::InputType::Number => Role::NumberInput,
                                    i_slint_core::items::InputType::Password => Role::PasswordInput,
                                    i_slint_core::items::InputType::Search => Role::SearchInput,
                                    i_slint_core::items::InputType::Text | _ => Role::TextInput,
                                }
                            }
                        } else {
                            Role::TextInput
                        }
                    }
                    i_slint_core::items::AccessibleRole::ProgressIndicator => {
                        Role::ProgressIndicator
                    }
                    i_slint_core::items::AccessibleRole::Switch => Role::Switch,
                    i_slint_core::items::AccessibleRole::ListItem => Role::ListBoxOption,
                    i_slint_core::items::AccessibleRole::Image => Role::Image,
                    i_slint_core::items::AccessibleRole::RadioButton => Role::RadioButton,
                    i_slint_core::items::AccessibleRole::RadioGroup => Role::RadioGroup,
                    i_slint_core::items::AccessibleRole::WindowTitleBar => Role::TitleBar,
                    i_slint_core::items::AccessibleRole::Banner => Role::Banner,
                    i_slint_core::items::AccessibleRole::Complementary => Role::Complementary,
                    i_slint_core::items::AccessibleRole::ContentInfo => Role::ContentInfo,
                    i_slint_core::items::AccessibleRole::Form => Role::Form,
                    i_slint_core::items::AccessibleRole::Main => Role::Main,
                    i_slint_core::items::AccessibleRole::Navigation => Role::Navigation,
                    i_slint_core::items::AccessibleRole::Region => Role::Region,
                    i_slint_core::items::AccessibleRole::Search => Role::Search,
                    _ => Role::Unknown,
                },
                item.accessible_string_property(
                    i_slint_core::accessibility::AccessibleStringProperty::Label,
                )
                .map(|x| x.to_string()),
            )
        };

        let mut node = Node::new(role);

        if let Some(label) = label {
            if role == Role::Label {
                node.set_value(label);
            } else {
                node.set_label(label);
            }
        }

        if item
            .accessible_string_property(AccessibleStringProperty::Enabled)
            .is_some_and(|x| x != "true")
        {
            node.set_disabled();
        }

        if item.borrow().as_ref().clips_children() {
            node.set_clips_children();
        }


        let is_checked = is_checkable
            && item
                .accessible_string_property(AccessibleStringProperty::Checked)
                .is_some_and(|x| x == "true");
        if is_checkable {
            node.set_toggled(if is_checked { Toggled::True } else { Toggled::False });
        }

        if let Some(description) =
            item.accessible_string_property(AccessibleStringProperty::Description)
        {
            node.set_description(description.to_string());
        }

        if let Some(id) = item.accessible_string_property(AccessibleStringProperty::Id) {
            node.set_author_id(id.to_string());
        }

        if item
            .accessible_string_property(AccessibleStringProperty::Expandable)
            .is_some_and(|x| x == "true")
        {
            node.set_expanded(
                item.accessible_string_property(AccessibleStringProperty::Expanded)
                    .is_some_and(|x| x == "true"),
            );
        }

        if matches!(
            role,
            Role::Button
                | Role::CheckBox
                | Role::ComboBox
                | Role::ListBoxOption
                | Role::MultilineTextInput
                | Role::NumberInput
                | Role::PasswordInput
                | Role::SearchInput
                | Role::Slider
                | Role::SpinButton
                | Role::Tab
                | Role::TextInput
        ) {
            node.add_action(Action::Focus);
        }

        if let Some(min) = item
            .accessible_string_property(AccessibleStringProperty::ValueMinimum)
            .and_then(|min| min.parse().ok())
        {
            node.set_min_numeric_value(min);
        }
        if let Some(max) = item
            .accessible_string_property(AccessibleStringProperty::ValueMaximum)
            .and_then(|max| max.parse().ok())
        {
            node.set_max_numeric_value(max);
        }
        if let Some(step) = item
            .accessible_string_property(AccessibleStringProperty::ValueStep)
            .and_then(|step| step.parse().ok())
        {
            node.set_numeric_value_step(step);
        }

        if let Some(value) = item.accessible_string_property(AccessibleStringProperty::Value) {
            match value.parse() {
                Ok(numeric) if role == Role::SpinButton => {
                    node.set_numeric_value(numeric);
                    node.set_value(value.to_string());
                }
                Ok(numeric) if !is_text_input_role(role) => node.set_numeric_value(numeric),
                _ => node.set_value(value.to_string()),
            }
        }

        if let Some(placeholder) =
            item.accessible_string_property(AccessibleStringProperty::PlaceholderText)
        {
            node.set_placeholder(placeholder.to_string());
        }

        if item
            .accessible_string_property(AccessibleStringProperty::ReadOnly)
            .is_some_and(|x| x == "true")
        {
            node.set_read_only();
        }

        if let Some(orientation) = item
            .accessible_string_property(AccessibleStringProperty::Orientation)
            .and_then(|s| s.parse::<i_slint_core::items::Orientation>().ok())
        {
            node.set_orientation(match orientation {
                i_slint_core::items::Orientation::Horizontal => Orientation::Horizontal,
                i_slint_core::items::Orientation::Vertical => Orientation::Vertical,
            });
        }

        if let Some(live) = item
            .accessible_string_property(AccessibleStringProperty::LiveRegion)
            .and_then(|s| s.parse::<i_slint_core::items::AccessibleLiveness>().ok())
        {
            node.set_live(match live {
                i_slint_core::items::AccessibleLiveness::Off => Live::Off,
                i_slint_core::items::AccessibleLiveness::Polite => Live::Polite,
                i_slint_core::items::AccessibleLiveness::Assertive => Live::Assertive,
                _ => Live::Off,
            });
        }

        if item
            .accessible_string_property(AccessibleStringProperty::ItemSelectable)
            .is_some_and(|x| x == "true")
        {
            node.set_selected(
                item.accessible_string_property(AccessibleStringProperty::ItemSelected)
                    .is_some_and(|x| x == "true"),
            );
        }

        if let Some(position_in_set) = item
            .accessible_string_property(AccessibleStringProperty::ItemIndex)
            .and_then(|s| s.parse::<usize>().ok())
        {
            node.set_position_in_set(position_in_set);
        }
        if let Some(size_of_set) = item
            .accessible_string_property(AccessibleStringProperty::ItemCount)
            .and_then(|s| s.parse::<usize>().ok())
        {
            node.set_size_of_set(size_of_set);
        }

        let supported = item.supported_accessibility_actions();
        if supported.contains(SupportedAccessibilityAction::Default) {
            node.add_action(accesskit::Action::Click);
        }
        if supported.contains(SupportedAccessibilityAction::Decrement) {
            node.add_action(accesskit::Action::Decrement);
        }
        if supported.contains(SupportedAccessibilityAction::Increment) {
            node.add_action(accesskit::Action::Increment);
        }
        if supported.contains(SupportedAccessibilityAction::SetValue) {
            node.add_action(accesskit::Action::SetValue);
        }
        if supported.contains(SupportedAccessibilityAction::ReplaceSelectedText) {
            node.add_action(accesskit::Action::ReplaceSelectedText);
        }
        if supported.contains(SupportedAccessibilityAction::Expand) {
            node.add_action(accesskit::Action::Expand);
        }

        node
    }
}

type AccessibilityPropertyTracker = PropertyTracker<true, AccessibilityPropertyDirtyHandler>;

struct AccessibilityPropertyDirtyHandler {
    window_adapter_weak: Weak<WinitWindowAdapter>,
}

impl i_slint_core::properties::PropertyDirtyHandler for AccessibilityPropertyDirtyHandler {
    fn notify(self: Pin<&Self>) {
        let win = self.window_adapter_weak.clone();
        i_slint_core::timers::Timer::single_shot(accessibility_update_delay(), move || {
            WinitWindowAdapter::with_access_kit_adapter_from_weak_window_adapter(
                win,
                |self_cell| {
                    self_cell.borrow_mut().rebuild_tree_of_dirty_nodes();
                },
            );
        })
    }
}

struct DelegateFocusPropertyTracker {
    window_adapter_weak: Weak<WinitWindowAdapter>,
}

impl i_slint_core::properties::PropertyDirtyHandler for DelegateFocusPropertyTracker {
    fn notify(self: Pin<&Self>) {
        let win = self.window_adapter_weak.clone();
        i_slint_core::timers::Timer::single_shot(Default::default(), move || {
            WinitWindowAdapter::with_access_kit_adapter_from_weak_window_adapter(
                win,
                |self_cell| {
                    self_cell.borrow_mut().handle_focus_item_change();
                },
            );
        })
    }
}

#[derive(Clone, Copy)]
enum CachedNodeKind {
    Item,
    /// The synthetic node under a Flickable that carries its scroll offset as a transform.
    FlickableContent,
}

struct CachedNode {
    id: NodeId,
    kind: CachedNodeKind,
    /// The Flickable content node whose coordinate space this node's bounds are in, if any.
    base: Option<NodeId>,
    children: Vec<NodeId>,
    window_position: LogicalPoint,
    /// Everything but geometry: role, label, value, selection, text runs.
    tracker: Pin<Box<PropertyTracker>>,
    /// Geometry and visibility only. During a scroll only this side goes dirty, and the node
    /// is re-sent only when its bounds in its own coordinate space actually changed.
    geometry_tracker: Pin<Box<PropertyTracker>>,
    /// The node as last sent to AccessKit; `None` until the first build.
    last: Option<Node>,
}

/// Debug counters, printed to stderr every second when SLINT_ACCESSKIT_STATS is set.
#[derive(Default)]
struct Stats {
    full: u32,
    incremental: u32,
    dirty_passes: u32,
    nodes_sent: u32,
    cached: u32,
    micros: u128,
    since: Option<std::time::Instant>,
}
thread_local! { static STATS: std::cell::RefCell<Option<Stats>> = std::cell::RefCell::new(
    std::env::var("SLINT_ACCESSKIT_STATS").is_ok().then(Stats::default)); }
fn stat(kind: &str, nodes_sent: usize, cached: usize, elapsed: std::time::Duration) {
    STATS.with(|st| {
        let mut st = st.borrow_mut();
        let Some(st) = st.as_mut() else { return };
        match kind { "full" => st.full += 1, "inc" => st.incremental += 1, _ => st.dirty_passes += 1 }
        st.nodes_sent += nodes_sent as u32;
        st.cached = cached as u32;
        st.micros += elapsed.as_micros();
        let now = std::time::Instant::now();
        let since = *st.since.get_or_insert(now);
        if now.duration_since(since) >= std::time::Duration::from_secs(1) {
            eprintln!("[accesskit] full={} incremental={} dirty={} nodes_sent={} cached={} cpu={:.1}ms",
                st.full, st.incremental, st.dirty_passes, st.nodes_sent, st.cached, st.micros as f64 / 1000.0);
            *st = Stats { since: Some(now), ..Default::default() };
        }
    });
}

/// Sub-index of a Flickable's content node, far above anything the TextRun children use.
const FLICKABLE_CONTENT_SUB_INDEX: u32 = NODE_ID_SUB_INDEX_MASK as u32;

/// The nearest Flickable on the way up from `item` (exclusive) to `ancestor` (inclusive).
fn nearest_flickable_below(item: &ItemRc, ancestor: &ItemRc) -> Option<ItemRc> {
    let mut current = item.parent_item(i_slint_core::item_tree::ParentItemTraversalMode::StopAtPopups);
    while let Some(candidate) = current {
        if candidate.downcast::<i_slint_core::items::Flickable>().is_some() {
            return Some(candidate);
        }
        if candidate == *ancestor {
            return None;
        }
        current = candidate.parent_item(i_slint_core::item_tree::ParentItemTraversalMode::StopAtPopups);
    }
    None
}

fn flickable_content_offset(flickable: &ItemRc) -> (f32, f32) {
    flickable
        .downcast::<i_slint_core::items::Flickable>()
        .map(|f| {
            let f = f.as_pin_ref();
            (f.content_x().0, f.content_y().0)
        })
        .unwrap_or_default()
}

impl From<accesskit_winit::Event> for SlintEvent {
    fn from(value: accesskit_winit::Event) -> Self {
        SlintEvent(crate::event_loop::CustomEvent::Accesskit(value))
    }
}

pub enum DeferredAccessKitAction {
    SetFocus(ItemRc),
    InvokeAccessibleAction(ItemRc, AccessibilityAction),
}

impl DeferredAccessKitAction {
    pub fn invoke(&self, window: &Window) {
        match self {
            DeferredAccessKitAction::SetFocus(item) => {
                // pretend this event was caused by a mouse for compatibility purposes
                WindowInner::from_pub(window).set_focus_item(item, true, FocusReason::PointerClick);
            }
            DeferredAccessKitAction::InvokeAccessibleAction(item, accessibility_action) => {
                item.accessible_action(accessibility_action);
            }
        }
    }
}

struct AccessiblePopup {
    location: LogicalPoint,
    parent_node: NodeId,
    component: ItemTreeRc,
}
