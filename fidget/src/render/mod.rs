//! 2D and 3D rendering
//!
//! To render something, build a configuration object then call its `run`
//! function, e.g. [`ImageRenderConfig::run`] and [`VoxelRenderConfig::run`].
use crate::{
    eval::{BulkEvaluator, Function, Trace, TracingEvaluator},
    shape::{Shape, ShapeTape},
    Error,
};
use std::sync::Arc;

mod config;
mod region;
mod render2d;
mod render3d;
mod view;

pub use config::{ImageRenderConfig, ThreadCount, VoxelRenderConfig};
pub use region::{ImageSize, RegionSize, VoxelSize};
pub use view::{View2, View3};

use render2d::render as render2d;
use render3d::render as render3d;

pub use render2d::{
    BitRenderMode, DebugRenderMode, RenderMode, SdfPixelRenderMode,
    SdfRenderMode,
};

/// A `RenderHandle` contains lazily-populated tapes for rendering
///
/// The tapes are stored as `Arc<..>`, so it can be cheaply cloned.
///
/// The most recent simplification is cached for reuse (if the trace matches).
pub struct RenderHandle<F: Function> {
    shape: Shape<F>,

    i_tape: Option<Arc<ShapeTape<<F::IntervalEval as TracingEvaluator>::Tape>>>,
    f_tape: Option<Arc<ShapeTape<<F::FloatSliceEval as BulkEvaluator>::Tape>>>,
    g_tape: Option<Arc<ShapeTape<<F::GradSliceEval as BulkEvaluator>::Tape>>>,

    next: Option<(F::Trace, Box<Self>)>,
}

impl<F: Function> Clone for RenderHandle<F> {
    fn clone(&self) -> Self {
        Self {
            shape: self.shape.clone(),
            i_tape: self.i_tape.clone(),
            f_tape: self.f_tape.clone(),
            g_tape: self.g_tape.clone(),
            next: None,
        }
    }
}

impl<F: Function> RenderHandle<F> {
    /// Build a new [`RenderHandle`] for the given shape
    ///
    /// None of the tapes are populated here.
    pub fn new(shape: Shape<F>) -> Self {
        Self {
            shape,
            i_tape: None,
            f_tape: None,
            g_tape: None,
            next: None,
        }
    }

    /// Returns a tape for tracing interval evaluation
    pub fn i_tape(
        &mut self,
        storage: &mut Vec<F::TapeStorage>,
    ) -> &ShapeTape<<F::IntervalEval as TracingEvaluator>::Tape> {
        self.i_tape.get_or_insert_with(|| {
            Arc::new(
                self.shape.interval_tape(storage.pop().unwrap_or_default()),
            )
        })
    }

    /// Returns a tape for bulk float evaluation
    pub fn f_tape(
        &mut self,
        storage: &mut Vec<F::TapeStorage>,
    ) -> &ShapeTape<<F::FloatSliceEval as BulkEvaluator>::Tape> {
        self.f_tape.get_or_insert_with(|| {
            Arc::new(
                self.shape
                    .float_slice_tape(storage.pop().unwrap_or_default()),
            )
        })
    }

    /// Returns a tape for bulk gradient evaluation
    pub fn g_tape(
        &mut self,
        storage: &mut Vec<F::TapeStorage>,
    ) -> &ShapeTape<<F::GradSliceEval as BulkEvaluator>::Tape> {
        self.g_tape.get_or_insert_with(|| {
            Arc::new(
                self.shape
                    .grad_slice_tape(storage.pop().unwrap_or_default()),
            )
        })
    }

    /// Simplifies the shape with the given trace
    ///
    /// As an internal optimization, this may reuse a previous simplification if
    /// the trace matches.
    pub fn simplify(
        &mut self,
        trace: &F::Trace,
        workspace: &mut F::Workspace,
        shape_storage: &mut Vec<F::Storage>,
        tape_storage: &mut Vec<F::TapeStorage>,
    ) -> &mut Self {
        // Free self.next if it doesn't match our new set of choices
        let mut trace_storage = if let Some(neighbor) = &self.next {
            if &neighbor.0 != trace {
                let (trace, neighbor) = self.next.take().unwrap();
                neighbor.recycle(shape_storage, tape_storage);
                Some(trace)
                // continue with simplification
            } else {
                None
            }
        } else {
            None
        };

        // Ordering is a little weird here, to persuade the borrow checker to be
        // happy about things.  At this point, `next` is empty if we can't reuse
        // it, and `Some(..)` if we can.
        if self.next.is_none() {
            let s = shape_storage.pop().unwrap_or_default();
            let next = self.shape.simplify(trace, s, workspace).unwrap();
            if next.size() >= self.shape.size() {
                // Optimization: if the simplified shape isn't any shorter, then
                // don't use it (this saves time spent generating tapes)
                shape_storage.extend(next.recycle());
                self
            } else {
                assert!(self.next.is_none());
                if let Some(t) = trace_storage.as_mut() {
                    t.copy_from(trace);
                } else {
                    trace_storage = Some(trace.clone());
                }
                self.next = Some((
                    trace_storage.unwrap(),
                    Box::new(RenderHandle {
                        shape: next,
                        i_tape: None,
                        f_tape: None,
                        g_tape: None,
                        next: None,
                    }),
                ));
                &mut self.next.as_mut().unwrap().1
            }
        } else {
            &mut self.next.as_mut().unwrap().1
        }
    }

    /// Recycles the entire handle into the given storage vectors
    pub fn recycle(
        mut self,
        shape_storage: &mut Vec<F::Storage>,
        tape_storage: &mut Vec<F::TapeStorage>,
    ) {
        // Recycle the child first, in case it borrowed from us
        if let Some((_trace, shape)) = self.next.take() {
            shape.recycle(shape_storage, tape_storage);
        }

        if let Some(i_tape) = self.i_tape.take() {
            if let Ok(i_tape) = Arc::try_unwrap(i_tape) {
                tape_storage.push(i_tape.recycle());
            }
        }
        if let Some(g_tape) = self.g_tape.take() {
            if let Ok(g_tape) = Arc::try_unwrap(g_tape) {
                tape_storage.push(g_tape.recycle());
            }
        }
        if let Some(f_tape) = self.f_tape.take() {
            if let Ok(f_tape) = Arc::try_unwrap(f_tape) {
                tape_storage.push(f_tape.recycle());
            }
        }

        // Do this step last because the evaluators may borrow the shape
        shape_storage.extend(self.shape.recycle());
    }
}

/// Container representing an ordered, checked list of tile sizes
///
/// This object wraps a `Vec<usize>`, guaranteeing three invariants:
///
/// - There must be at least one tile size
/// - Tiles must be ordered from largest to smallest
/// - Each tile size must be exactly divisible by subsequent tile sizes
#[derive(Debug, Eq, PartialEq)]
pub struct TileSizes(Vec<usize>);

impl std::ops::Index<usize> for TileSizes {
    type Output = usize;

    fn index(&self, i: usize) -> &Self::Output {
        &self.0[i]
    }
}

impl TileSizes {
    /// Builds a new tile size list, checking invariants
    pub fn new(sizes: &[usize]) -> Result<Self, Error> {
        if sizes.is_empty() {
            return Err(Error::EmptyTileSizes);
        }
        for i in 1..sizes.len() {
            if sizes[i - 1] <= sizes[i] {
                return Err(Error::BadTileOrder(sizes[i - 1], sizes[i]));
            } else if sizes[i - 1] % sizes[i] != 0 {
                return Err(Error::BadTileSize(sizes[i - 1], sizes[i]));
            }
        }
        Ok(Self(sizes.to_vec()))
    }
    /// Returns the length of the tile list
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns an iterator over tile sizes (largest to smallest)
    pub fn iter(&self) -> impl Iterator<Item = &usize> {
        self.0.iter()
    }

    /// Returns the last (smallest) tile size
    pub fn last(&self) -> usize {
        *self.0.last().unwrap()
    }

    /// Gets a tile size by index
    pub fn get(&self, i: usize) -> Option<usize> {
        self.0.get(i).copied()
    }

    /// Returns the data offset of a global pixel position within a root tile
    ///
    /// The root tile is implicit: it's set by the largest tile size and aligned
    /// to multiples of that size.
    #[inline]
    pub(crate) fn pixel_offset(&self, pos: nalgebra::Point2<usize>) -> usize {
        // Find the relative position within the root tile
        let x = pos.x % self.0[0];
        let y = pos.y % self.0[0];

        // Apply the relative offset and find the data index
        x + y * self.0[0]
    }
}

/// Hints for how to render this particular type
pub trait RenderHints {
    /// Recommended tile sizes for 3D rendering
    fn tile_sizes_3d() -> TileSizes;

    /// Recommended tile sizes for 2D rendering
    fn tile_sizes_2d() -> TileSizes;

    /// Indicates whether we run tape simplification at the given cell depth
    /// during meshing.
    ///
    /// By default, this is always true; for evaluators where simplification is
    /// more expensive than evaluation (i.e. the JIT), it may only be true at
    /// certain depths.
    fn simplify_tree_during_meshing(_d: usize) -> bool {
        true
    }
}
