use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use rstar::{
    primitives::{GeomWithData, Line},
    RTree,
};
use tokio::{sync::Mutex, task::JoinHandle};
use ultraviolet::Vec2;
use waragraph_core::graph::{Bp, PathId, PathIndex};

use crate::annotations::AnnotationId;

use super::view::View1D;

/// Rendering annotations into 1D viewer slots

#[derive(Default, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AnnotSlotId(pub(super) u32);

#[derive(Default)]
pub struct Annots1D {
    slots: HashMap<AnnotSlotId, AnnotSlot>,
    next_slot_id: AnnotSlotId,

    path_annot_slot: HashMap<PathId, AnnotSlotId>,
}

impl Annots1D {
    pub fn get_path_slot_id(&self, path: PathId) -> Option<AnnotSlotId> {
        let slot = self.path_annot_slot.get(&path)?;
        Some(*slot)
    }

    pub fn insert_slot(
        &mut self,
        path: PathId,
        slot: AnnotSlot,
    ) -> AnnotSlotId {
        let slot_id = self.next_slot_id;
        self.slots.insert(slot_id, slot);
        self.path_annot_slot.insert(path, slot_id);
        self.next_slot_id = AnnotSlotId(slot_id.0 + 1);
        slot_id
    }

    pub fn get(&self, slot_id: &AnnotSlotId) -> Option<&AnnotSlot> {
        self.slots.get(slot_id)
    }

    pub fn get_mut(&mut self, slot_id: &AnnotSlotId) -> Option<&mut AnnotSlot> {
        self.slots.get_mut(slot_id)
    }
}

type AnnotsTreeObj = GeomWithData<Line<(i64, i64)>, AnnotationId>;

type ShapeFn = Box<dyn Fn(&egui::Painter, egui::Pos2) -> egui::Shape>;

pub fn text_shape<L: ToString>(label: L) -> ShapeFn {
    let label = label.to_string();
    Box::new(move |painter, pos| {
        let fonts = painter.fonts();
        let font = egui::FontId::proportional(16.0);
        let color = egui::Color32::WHITE;
        egui::Shape::text(
            &fonts,
            pos,
            egui::Align2::CENTER_CENTER,
            label.clone(),
            font,
            color,
        )
    })
}

// Container for annotations displayed in a single 1D slot,
// with the annotations "flattened" to the pangenome coordinate
// space, down from the path-range space
pub struct AnnotSlot {
    // id: AnnotSlotId
    // really corresponds to the anchor regions
    annots: Arc<RTree<AnnotsTreeObj>>,

    shape_fns: Vec<ShapeFn>,

    dynamics: Arc<Mutex<AnnotSlotDynamics>>,

    task: Option<JoinHandle<Vec<(AnnotationId, Vec2)>>>,

    // pair of (annot_id, pos) as produced by task; first value is used as key to shape_fn
    positions: Vec<(AnnotationId, Vec2)>,

    // pair of (annot_id, shape size) as produced by rendering
    shape_sizes: Vec<(AnnotationId, Vec2)>,
}

#[derive(Default)]
struct AnnotSlotDynamics {
    // annot id -> annot_shape_objs ix
    annot_obj_map: HashMap<AnnotationId, usize>,
    annot_shape_objs: Vec<AnnotObj>,

    deltas: Vec<Vec2>,

    cur_view: Option<View1D>,
    prev_view: Option<View1D>,

    visible_set: BTreeSet<AnnotationId>,
    // visible_set: HashSet<AnnotationId>,
}

#[derive(Debug, Clone, Copy)]
struct AnnotObj {
    annot_id: AnnotationId,

    // pos: AnnotObjPos,
    anchor_target_pos: Option<f32>,
    anchor_pos: Option<f32>,

    // closest_anchor_pos: Option<f32>,
    // anchor_pos: Option<AnnotObjPos>,
    shape_size: Option<Vec2>,
}

#[derive(Debug, Clone, Copy)]
struct AnnotObjPos {
    pos_now: Vec2,
    pos_old: Vec2,
    accel: Vec2,
}

impl AnnotSlotDynamics {
    fn get_annot_obj(&self, a_id: AnnotationId) -> Option<&AnnotObj> {
        let i = *self.annot_obj_map.get(&a_id)?;
        Some(&self.annot_shape_objs[i])
    }

    fn get_annot_obj_mut(
        &mut self,
        a_id: AnnotationId,
    ) -> Option<&mut AnnotObj> {
        let i = *self.annot_obj_map.get(&a_id)?;
        Some(&mut self.annot_shape_objs[i])
    }

    fn get_or_insert_annot_obj_mut(
        &mut self,
        a_id: AnnotationId,
        // pos: Vec2,
    ) -> &mut AnnotObj {
        if let Some(i) = self.annot_obj_map.get(&a_id) {
            &mut self.annot_shape_objs[*i]
        } else {
            let obj_i = self.annot_shape_objs.len();
            let obj = AnnotObj::empty(a_id);
            self.annot_obj_map.insert(a_id, obj_i);
            self.annot_shape_objs.push(obj);
            &mut self.annot_shape_objs[obj_i]
        }
    }

    fn prepare(
        &mut self,
        annots: &RTree<AnnotsTreeObj>,
        screen_rect: egui::Rect,
        view: &View1D,
    ) {
        use rstar::AABB;
        let rleft = screen_rect.left();
        let rright = screen_rect.right();

        let screen_interval = rleft..=rright;

        let range = view.range();

        let aabb =
            AABB::from_corners((range.start as i64, 0), (range.end as i64, 0));

        let in_view = annots.locate_in_envelope_intersecting(&aabb);

        self.prev_view = self.cur_view.clone();
        self.cur_view = Some(view.clone());

        let mut annot_ranges: HashMap<AnnotationId, Vec<_>> =
            HashMap::default();

        // let mut annot_reset_pos: HashSet<_> = HashSet::default();

        // collect the visible annotations
        for line in in_view {
            let a_id = line.data;
            let left = line.geom().from.0 as u64;
            let right = line.geom().to.0 as u64;

            if let Some(anchor_range) =
                anchor_interval(view, &(left..right), &screen_interval)
            {
                annot_ranges
                    .entry(a_id)
                    .or_default()
                    .push(anchor_range.clone());
            }
        }

        self.visible_set.clear();

        use rand::distributions::WeightedIndex;
        use rand::prelude::*;
        let mut rng = rand::thread_rng();

        for (&a_id, ranges) in annot_ranges.iter() {
            // if the annotation has no object, create it
            let obj = self.get_or_insert_annot_obj_mut(a_id);

            // if there's already an anchor target on this object,
            // constrain it to the visible anchor set

            if let Some(tgt) = obj.anchor_target_pos.as_mut() {
                let mut dist = std::f32::INFINITY;
                let mut closest_tgt = None;

                for range in ranges {
                    // if the current target is already on one of the ranges,
                    // we're done
                    if *tgt >= *range.start() && *tgt <= *range.end() {
                        // dist = 0.0;
                        closest_tgt = Some(*tgt);
                        break;
                    }

                    let closest = if *tgt < *range.start() {
                        *range.start()
                    } else if *tgt > *range.end() {
                        *range.end()
                    } else {
                        unreachable!();
                    };

                    let new_dist = (closest - *tgt).abs();
                    if new_dist < dist {
                        closest_tgt = Some(closest);
                        dist = new_dist;
                    }
                }

                if let Some(new_tgt) = closest_tgt {
                    *tgt = new_tgt;
                }
            } else {
                let (ranges, lens): (Vec<_>, Vec<_>) =
                    ranges.iter().map(|r| (r, r.end() - r.start())).unzip();

                // if the ann. object has no anchor target,
                // choose a random position from the intersection of view
                // with the anchor ranges (across all visible sections) &
                // set the anchor target position to that point

                let dist = WeightedIndex::new(&lens).unwrap();

                let ix = rng.sample(dist);
                let anchor_target = rng.gen_range(ranges[ix].clone());

                obj.anchor_target_pos = Some(anchor_target);
            }

            if obj.anchor_target_pos.is_some() {
                self.visible_set.insert(a_id);
            }
        }
    }

    /*
    fn prepare_old(
        &mut self,
        annots: &RTree<AnnotsTreeObj>,
        screen_rect: egui::Rect,
        view: &View1D,
    ) {
        // initialize AnnotObjPos for the annotations in the view
        // treat X as anchor X; use separate objects with spring constraint later

        use rstar::AABB;
        let rleft = screen_rect.left();
        let rright = screen_rect.right();

        let screen_interval = rleft..=rright;

        let range = view.range();

        let aabb =
            AABB::from_corners((range.start as i64, 0), (range.end as i64, 0));

        let in_view = annots.locate_in_envelope_intersecting(&aabb);

        self.prev_view = self.cur_view.clone();
        self.cur_view = Some(view.clone());

        let view_changed =
            self.prev_view != self.cur_view && self.cur_view.is_some();

        /*
        if view_changed, use prev_view & cur_view to derive the delta
        transformation to be applied to the (screenspace) annotation
        positions
        */

        let shape_bin_width = 8usize;
        let shape_bin_count =
            (screen_rect.width() / shape_bin_width as f32).round() as usize;
        let mut shape_bins = vec![0usize; shape_bin_count];

        for line in in_view {
            let a_id = line.data;
            let left = line.geom().from.0 as u64;
            let right = line.geom().to.0 as u64;

            let reset_pos =
                if let Some(pos) = self.get_annot_obj(a_id).map(|o| o.pos) {
                    pos.pos_now.x < rleft || pos.pos_now.x > rright
                } else {
                    true
                };

            let anchor_range =
                anchor_interval(view, &(left..right), &screen_interval);

            if reset_pos {
                if let Some(a_range) = anchor_range.as_ref() {
                    let (l, r) = a_range.clone().into_inner();
                    let x = l + (r - l) * 0.5;

                    // let y = screen_rect.center().y + 20.0 * (x * 10.0).sin();
                    let y = screen_rect.center().y + 20.0 * x.sin();

                    let _obj =
                        self.get_or_insert_annot_obj_mut(a_id, Vec2::new(x, y));
                }
            } else if view_changed {
                // only apply the view transform to annots that haven't been reset

                let transform = self
                    .prev_view
                    .as_ref()
                    .and_then(|v0| Some((v0, self.cur_view.as_ref()?)))
                    .map(|(v0, v1)| {
                        super::Viewer1D::sample_index_transform(
                            v0.range(),
                            v1.range(),
                        )
                    });

                if let Some([a, b]) = transform {
                    let w = screen_rect.width();
                    let x0 = screen_rect.left();

                    let apply_tf = |p: &mut Vec2| {
                        let x = p.x - x0;
                        let x_ = x * a - w * b;
                        p.x = x_ + x0;
                    };

                    if let Some(obj) = self.get_annot_obj_mut(a_id) {
                        apply_tf(&mut obj.pos.pos_now);
                        // apply_tf(&mut obj.pos.pos_old);
                    }
                }
            }

            {
                // TODO: update the closest point to the anchor set from the
                // annotation's current position

                if let Some((a_range, obj)) = self
                    .get_annot_obj_mut(a_id)
                    .and_then(|obj| Some((anchor_range?, obj)))
                {
                    let (left, right) = a_range.into_inner();
                    let range_x = left + (right - left) * 0.5;
                    let obj_x = obj.pos.pos_now.x;

                    let dist = (range_x - obj_x).abs();

                    if let Some(cur_closest) = obj.closest_anchor_pos.as_mut() {
                        if (*cur_closest - obj_x).abs() > dist {
                            *cur_closest = range_x;
                        }
                    } else {
                        obj.closest_anchor_pos = Some(range_x);
                    }
                };
            }

            // update shape bins
            if let Some((pos, size)) = self.get_annot_obj(a_id).and_then(|o| {
                let pos = o.pos.pos_now;
                let size = o.size()?;
                Some((pos, size))
            }) {
                let w2 = size.x / 2.0;
                let x = pos.x - screen_rect.left();
                let left = x - w2;
                let right = x + w2;

                let bin_w = shape_bin_width as f32;

                let li = (left / bin_w).round() as usize;
                let ri = (right / bin_w).round() as usize;
                let ri = ri.clamp(0, shape_bins.len());
                let li = li.clamp(0, ri);

                let bin_range = li..ri;

                shape_bins[bin_range].iter_mut().for_each(|c| *c += 1);
            }
        }
    }
    */

    fn update_simple(
        &mut self,
        screen_rect: egui::Rect,
        dt: f32,
    ) -> Vec<(AnnotationId, Vec2)> {
        use iset::IntervalMap;

        let objs_n = self.annot_shape_objs.len();

        // let mut objs = (0..objs_n)
        //     .map(|i| (i, &self.annot_shape_objs[i]))
        //     .collect::<Vec<_>>();
        // objs.sort_by_key(|(_, o)| o.annot_id);

        // NB: this might get weird... maybe i want to store the last
        // updated view for each object, and use that to compute the
        // transform -- but that's only if this ends up not working
        let transform = self
            .prev_view
            .as_ref()
            .and_then(|v0| Some((v0, self.cur_view.as_ref()?)))
            .filter(|(v0, v1)| v0 != v1) // no need to transform if there's no change in view
            .map(|(v0, v1)| {
                super::Viewer1D::sample_index_transform(v0.range(), v1.range())
            });

        let mut placed_labels: IntervalMap<f32, usize> = IntervalMap::default();

        for &annot_id in &self.visible_set {
            let obj_i = self.annot_obj_map[&annot_id];
            let obj = &mut self.annot_shape_objs[obj_i];

            let width = if let Some(size) = obj.size() {
                size.x
            } else {
                1.0
            };

            // if the object has an anchor target (i guess they always
            // will, at least the visible set),

            // if it doesn't have an anchor position, set it to the target

            // if it does have an anchor position, we may need to update it
            //  - if the anchor is further than some given distance from its target,
            //    move the anchor toward the target

            // then lay out the labels horizontally in the interval map; skip once they start overlapping

            if let Some([a, b]) = transform {
                // apply view transform to labels if applicable

                let apply_tf = |val: Option<&mut f32>| {
                    let w = screen_rect.width();
                    let x0 = screen_rect.left();

                    if let Some(x) = val {
                        let v = *x - x0;
                        let v_ = v * a - w * b;
                        *x = v_ + x0;
                    }
                };

                apply_tf(obj.anchor_target_pos.as_mut());
                apply_tf(obj.anchor_pos.as_mut());
            }
        }

        //

        todo!();

        let mut positions = Vec::with_capacity(objs_n);

        // for (_range, obj_i) in placed_labels.into_iter(..) {
        //     let obj = &mut self.annot_shape_objs[obj_i];

        //     let annot_id = obj.annot_id;
        //     positions.push((annot_id, obj.pos.pos_now));

        //     obj.closest_anchor_pos = None;
        // }

        positions

        /*
        for (obj_i, obj) in objs {
            let ival = if let Some(rect) = obj.egui_rect() {
                rect.left()..rect.right()
            } else if let Some(anchor) = obj.closest_anchor_pos {
                // if the annotation hasn't been rendered yet, but has
                // a position, give it a placeholder interval
                let x = anchor;
                let l = x - 0.5;
                let r = x + 0.5;
                l..r
            } else {
                continue;
            };

            let overlaps = placed_labels.has_overlap(ival.clone());

            if !overlaps {
                placed_labels.insert(ival, obj_i);
            }
        }

        let mut positions = Vec::with_capacity(objs_n);

        for (_range, obj_i) in placed_labels.into_iter(..) {
            let obj = &mut self.annot_shape_objs[obj_i];

            let annot_id = obj.annot_id;
            positions.push((annot_id, obj.pos.pos_now));

            obj.closest_anchor_pos = None;
        }

        positions
        */
        // log::warn!("pushing {} annotation labels", positions.len());
    }

    /*
    fn update(
        &mut self,
        screen_rect: egui::Rect,
        dt: f32,
    ) -> Vec<(AnnotationId, Vec2)> {
        let objs = self.annot_shape_objs.len();

        self.deltas.clear();
        self.deltas.resize(objs, Vec2::zero());

        for i in 0..objs {
            for j in 0..objs {
                if i == j {
                    continue;
                }

                let (rect_i, rect_j) = {
                    let ri = self.annot_shape_objs[i].egui_rect();
                    let rj = self.annot_shape_objs[j].egui_rect();

                    if let (Some(ri), Some(rj)) = (ri, rj) {
                        (ri, rj)
                    } else {
                        continue;
                    }
                };

                let delta = AnnotObj::intersect_delta(rect_i, rect_j);

                // if delta.y.abs() > 0.0 {
                //     log::warn!("obj {i} - {delta:?}");
                // }

                self.annot_shape_objs[i].pos.pos_now += delta;

                // obj.pos.pos_now += delta;
                // self.deltas[i] += delta;
            }
        }

        let mut positions = Vec::with_capacity(objs);

        for (_obj_i, (&delta, obj)) in self
            .deltas
            .iter()
            .zip(self.annot_shape_objs.iter_mut())
            .enumerate()
        {
            // obj.pos.pos_now += delta * dt;
            // obj.pos.pos_now += delta;
            // obj.pos.accel = delta;

            // gravity
            obj.pos.accel.y += 10.0;
            let v = obj.pos.pos_now - obj.pos.pos_old;

            // apply anchor constraint
            if let Some(anchor) = obj.closest_anchor_pos {
                // let dist = anchor - obj.pos.pos_now.x;
                obj.pos.pos_now.x = anchor;
                /*
                let dist = obj.pos.pos_now.x - anchor;

                if dist.abs() > 1.0 {
                    // let k = 1.0;
                    // let k = 0.1;
                    // let k = 0.5;
                    let k = 0.5;

                    let f = -k * dist;
                    let a = f;

                    obj.pos.accel.x += a;
                } else {
                    let damp = 10.0;
                    obj.pos.accel.x = -v.x * damp;
                }
                */

                // obj.pos.pos_now.x = anchor;
                // obj.closest_anchor_pos = None;

                let annot_id = obj.annot_id;
                positions.push((annot_id, obj.pos.pos_now));
            }

            // TODO: disabled until the collision & update_position behave correctly
            // obj.pos.update_position(dt);

            if let Some(rect) = obj.egui_rect() {
                if rect.bottom() > screen_rect.bottom() {
                    // obj.pos.pos_now.y = screen_rect.bottom() - rect.height();
                    obj.pos.pos_now.y -= rect.bottom() - screen_rect.bottom();
                }
            }

            // reset
            obj.closest_anchor_pos = None;
        }

        positions
    }
    */
}

impl AnnotObjPos {
    fn at_pos(pos: Vec2) -> Self {
        Self {
            pos_now: pos,
            pos_old: pos,
            accel: Vec2::zero(),
        }
    }
    fn update_position(&mut self, dt: f32) {
        let vel = self.pos_now - self.pos_old;
        self.pos_old = self.pos_now;
        self.pos_now = self.pos_now + vel + self.accel * dt * dt;
        self.accel = Vec2::zero();
    }

    fn accelerate(&mut self, acc: Vec2) {
        self.accel += acc;
    }
}

impl AnnotObj {
    fn empty(annot_id: AnnotationId) -> Self {
        Self {
            annot_id,
            // pos: None,
            // closest_anchor_pos: None,
            shape_size: None,

            anchor_target_pos: None,
            anchor_pos: None,
        }
    }

    // fn with_pos(annot_id: AnnotationId, pos: Vec2) -> Self {
    //     Self {
    //         annot_id,
    //         pos: Some(AnnotObjPos::at_pos(pos)),
    //         closest_anchor_pos: None,
    //         shape_size: None,

    //         anchor_target_pos: None,
    //         anchor_pos: None,
    //     }
    // }

    // fn pos(&self) -> Vec2 {
    //     self.pos.pos_now
    // }

    fn size(&self) -> Option<Vec2> {
        self.shape_size
    }

    /*
    fn egui_rect(&self) -> Option<egui::Rect> {
        let pos = self.pos();
        let size = self.size()?;
        let rect = egui::Rect::from_center_size(
            egui::pos2(pos.x, pos.y),
            egui::vec2(size.x, size.y),
        );
        Some(rect)
    }
    */

    // fn collides_impl(&self, other: &Self) -> Option<bool> {
    // let a = self.egui_rect()?;
    // let b = other.egui_rect()?;
    // Some(a.intersects(b))
    // }

    // fn collides(&self, other: &Self) -> bool {
    //     self.collides_impl(other).unwrap_or(false)
    // }

    // outputs the delta that when applied to `this` resolves half of
    // the collision between the two
    fn intersect_delta(this: egui::Rect, other: egui::Rect) -> Vec2 {
        if !this.intersects(other) {
            return Vec2::zero();
        }

        let t_center: Vec2 = mint::Point2::from(this.center()).into();
        let o_center: Vec2 = mint::Point2::from(this.center()).into();
        let diff = t_center - o_center;

        let intersection = this.intersect(other);

        if diff.x.abs() < diff.y.abs() {
            let dx = if diff.x < 0.0 {
                //   `this` left of `other`
                -intersection.width()
            } else {
                //   `this` right of `other`
                intersection.width()
            };

            let dx = dx * 0.5;

            Vec2::new(dx, 0.0)
        } else {
            let dy = if diff.y < 0.0 {
                //   `this` above `other`
                -intersection.height()
            } else {
                //   `this` below `other`
                intersection.height()
            };

            let dy = dy * 0.5;

            Vec2::new(0.0, dy)
        }
    }
}

impl AnnotSlot {
    /// Initializes an annotation slot given items in pangenome space.
    ///
    pub fn new_from_pangenome_space(
        annotations: impl IntoIterator<Item = (std::ops::Range<Bp>, ShapeFn)>,
    ) -> Self {
        let mut annot_objs = Vec::new();
        let mut shape_fns = Vec::new();

        for (a_id, (range, shape)) in annotations.into_iter().enumerate() {
            let a_id = AnnotationId(a_id);
            let geom =
                Line::new((range.start.0 as i64, 0), (range.end.0 as i64, 0));
            annot_objs.push(GeomWithData::new(geom, a_id));
            shape_fns.push(shape);
        }

        let annots = RTree::<AnnotsTreeObj>::bulk_load(annot_objs);

        Self {
            annots: Arc::new(annots),
            shape_fns,
            dynamics: Default::default(),
            task: None,
            positions: Vec::new(),
            shape_sizes: Vec::new(),
        }
    }

    /// Initializes an annotation slot given items in path space.
    /// The path ranges to pangenome space, splitting them if
    /// necessary.
    pub fn new_from_path_space(
        graph: &PathIndex,
        annotations: impl IntoIterator<
            Item = (PathId, std::ops::Range<Bp>, ShapeFn),
        >,
    ) -> Self {
        let mut annot_objs = Vec::new();
        let mut shape_fns = Vec::new();

        for (a_id, (path, path_range, shape)) in
            annotations.into_iter().enumerate()
        {
            let a_id = AnnotationId(a_id);
            shape_fns.push(shape);
            let range_end = path_range.end;
            if let Some(steps) = graph.path_step_range_iter(path, path_range) {
                for (start, step) in steps {
                    let len = graph.node_length(step.node()).0 as usize;
                    let end = (start + len).min(range_end.0 as usize);
                    let geom = Line::new((start as i64, 0), (end as i64, 0));
                    annot_objs.push(GeomWithData::new(geom, a_id));
                }
            }
        }

        let annots = RTree::<AnnotsTreeObj>::bulk_load(annot_objs);

        Self {
            annots: Arc::new(annots),
            shape_fns,
            dynamics: Default::default(),
            task: None,
            positions: Vec::new(),
            shape_sizes: Vec::new(),
        }
    }

    pub(super) fn update(
        &mut self,
        rt: &tokio::runtime::Handle,
        screen_rect: egui::Rect,
        view: &View1D,
        dt: f32,
    ) {
        if let Some(handle) = self.task.take() {
            // if done, update the stored positions
            if handle.is_finished() {
                if let Ok(positions) = rt.block_on(handle) {
                    self.positions = positions;
                }
            } else {
                self.task = Some(handle);
            }
        } else {
            self.update_spawn_task(rt, screen_rect, view, dt);
        }
    }

    fn update_spawn_task(
        &mut self,
        rt: &tokio::runtime::Handle,
        screen_rect: egui::Rect,
        view: &View1D,
        dt: f32,
    ) {
        if self.task.is_some() {
            return;
        }

        {
            let mut dynamics = self.dynamics.blocking_lock();

            for &(a_id, size) in self.shape_sizes.iter() {
                if let Some(obj) = dynamics.get_annot_obj_mut(a_id) {
                    obj.shape_size = Some(size);
                }
            }
        }

        let annots_tree = self.annots.clone();
        let dynamics = self.dynamics.clone();
        let view = view.clone();

        // spawn the task
        let handle = rt.spawn(async move {
            let mut dynamics = dynamics.lock().await;
            dynamics.prepare(&annots_tree, screen_rect, &view);
            dynamics.update_simple(screen_rect, dt)
            // dynamics.update(screen_rect, dt)
        });

        self.task = Some(handle);
    }

    pub(super) fn draw(&mut self, painter: &egui::Painter, view: &View1D) {
        self.shape_sizes.clear();

        for &(a_id, pos) in self.positions.iter() {
            let pos = mint::Point2::<f32>::from(pos);
            let shape = self.shape_fns[a_id.0](painter, pos.into());
            let size =
                mint::Vector2::<f32>::from(shape.visual_bounding_rect().size());
            self.shape_sizes.push((a_id, size.into()));

            painter.add(shape);
        }
    }
}

// returns the range of valid anchor points along the x-axis, in
// screen space, of the `pan_range` range under the transformation
// induced by the `view` and `screen_interval`
//
// if the intersection of `view` and `pan_range` is empty, None is returned
fn anchor_interval(
    view: &View1D,
    pan_range: &std::ops::Range<u64>,
    screen_interval: &std::ops::RangeInclusive<f32>,
) -> Option<std::ops::RangeInclusive<f32>> {
    let vrange = view.range();
    let pleft = pan_range.start;
    let pright = pan_range.end;

    if pleft > vrange.end || pright < vrange.start {
        return None;
    }

    let vl = vrange.start as f32;
    let vr = vrange.end as f32;
    let vlen = vr - vl;

    let left = pleft.max(vrange.start);
    let right = pright.min(vrange.end);

    let l = left as f32;
    let r = right as f32;

    let lt = (l - vl) / vlen;
    let rt = (r - vl) / vlen;

    let (sleft, sright) = screen_interval.clone().into_inner();
    let slen = sright - sleft;

    let a_left = sleft + lt * slen;
    let a_right = sleft + rt * slen;

    Some(a_left..=a_right)
}

pub(crate) mod util {
    use waragraph_core::graph::{Bp, Node};

    use super::*;

    pub(crate) fn label_nodes<L: ToString>(
        graph: &PathIndex,
        labels: impl IntoIterator<Item = (Node, L)>,
    ) -> AnnotSlot {
        let annots = labels.into_iter().map(|(node, label)| {
            let node_range = graph.node_pangenome_range(node);
            (node_range, text_shape(label))
        });

        AnnotSlot::new_from_pangenome_space(annots)
    }

    pub(crate) fn pangenome_range_labels<L: ToString>(
        labels: impl IntoIterator<Item = (std::ops::Range<Bp>, L)>,
    ) -> AnnotSlot {
        let annots = labels
            .into_iter()
            .map(|(range, label)| (range, text_shape(label)));
        AnnotSlot::new_from_pangenome_space(annots)
    }
}
