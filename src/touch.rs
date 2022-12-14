//! Driver for the official touchscreen.
//!
//! There is no official documentation for this driver, so its implementation is my interpretation of the implementation in the [Linux kernel source](https://github.com/raspberrypi/linux/blob/rpi-5.15.y/drivers/input/touchscreen/raspberrypi-ts.c).

extern crate alloc;

use alloc::boxed::Box;
use core::cmp::min;
use core::mem::MaybeUninit;
use core::sync::atomic::{fence, Ordering};

use crate::alloc::{Shell as Allocator, DMA};
use crate::irq::IRQ;
use crate::math::{Normal, Quaternion, Scalar, Vector};
use crate::mbox::{Request, RequestProperty, MBOX};
use crate::sync::{Lazy, Lock, RwLock};

/// Video IRQ which we piggyback on since the touchscreen has no IRQ of its own.
const TOUCH_IRQ: u32 = 142;
/// Maximum number of touch points tracked by the video core.
const MAX_POINTS: usize = 10;
/// Invalid points length used by the VC as a poor man's lock.
const INVALID_POINTS: u8 = 99;
/// Touch sensor's width.
const WIDTH: i16 = 800;
/// Touch sensor's height.
const HEIGHT: i16 = 480;

/// Global touchscreen driver instance.
pub static TOUCH: Lazy<Touch> = Lazy::new(Touch::new);

/// Touchscreen driver.
#[derive(Debug)]
pub struct Touch
{
    /// Touchscreen buffer.
    state: Lock<Box<State, Allocator<'static>>>,
    /// Saved touch points for comparison.
    saved: RwLock<Option<(Vector, Vector)>>,
}

/// Input changes since the last poll.
#[derive(Clone, Copy, Debug)]
pub struct Recognizer
{
    /// Last saved sample.
    saved: Option<(Vector, Vector)>,
    /// Amount moved since the last poll.
    pub trans: Vector,
    /// Amount rotated since the last poll.
    pub rot: Quaternion,
}

/// Touchscreen state information from the video core.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
struct State
{
    /// Not sure about the purpose of this field.
    _mode: u8,
    /// Not sure about the purpose of this field.
    _gesture: u8,
    /// Number of touch points in the buffer.
    points_len: u8,
    /// Information about individual touch points.
    points: [Point; MAX_POINTS],
}

/// Information about an individual point.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
struct Point
{
    /// Most significant byte of the horizontal coordinate.
    x_msb: u8,
    /// Least significant byte of the horizontal coordinate.
    x_lsb: u8,
    /// Most significant byte of the vertical coordinate.
    y_msb: u8,
    /// Least significant byte of the vertical coordinate.
    y_lsb: u8,
    /// Touch force (unused).
    _force: u8,
    /// Touch area (unused).
    _area: u8,
}

impl Touch
{
    /// Creates and initializes a new touchscreen driver.
    ///
    /// Returns the initialized touchscreen driver.
    fn new() -> Self
    {
        #[allow(invalid_value)] // Filled by the hardware.
        #[allow(clippy::uninit_assumed_init)] // Same as above.
        let mut state = unsafe { MaybeUninit::<State>::uninit().assume_init() };
        state.points_len = INVALID_POINTS;
        let mut state = Box::new_in(state, DMA);
        let mut req = Request::new();
        req.push(RequestProperty::SetTouchBuffer { buf: state.as_mut() as *mut State as _ });
        MBOX.exchange(req);
        let saved = None;
        IRQ.register(TOUCH_IRQ, Self::poll);
        Self { state: Lock::new(state),
               saved: RwLock::new(saved) }
    }

    /// Handler that polls the touchscreen buffer and updates the saved state
    /// when new information is available.
    fn poll()
    {
        fence(Ordering::Acquire);
        let mut hw_state = TOUCH.state.lock();
        let state = **hw_state;
        if state.points_len as usize > MAX_POINTS {
            return;
        }
        hw_state.points_len = INVALID_POINTS;
        fence(Ordering::Release);
        drop(hw_state);
        // We're only interested in information containing two touch points.
        if state.points_len != 2 {
            *TOUCH.saved.wlock() = None;
            return;
        }
        let mapper = |point: Point| {
            let x = point.x_lsb as i16 | (point.x_msb as i16 & 0x3) << 8;
            let y = point.y_lsb as i16 | (point.y_msb as i16 & 0x3) << 8;
            let x = x * 2 - WIDTH;
            let y = y * 2 - HEIGHT;
            let x = x as f32 / min(WIDTH, HEIGHT) as f32;
            let y = y as f32 / min(WIDTH, HEIGHT) as f32;
            Vector::from_components(x, y, 0.0)
        };
        let new = state.points.map(mapper);
        let new = (new[0], new[1]);
        *TOUCH.saved.wlock() = Some(new);
    }
}

impl Recognizer
{
    /// Creates and initializes a new gesture recognizer.
    ///
    /// Returns the newly created recognizer.
    pub fn new() -> Self
    {
        Self { saved: None,
               trans: Vector::default(),
               rot: Quaternion::default() }
    }

    /// Returns the amount translated since the last sample.
    pub fn translated(&self) -> Vector
    {
        self.trans
    }

    /// Returns the amount rotated since last sampled.
    pub fn rotated(&self) -> Quaternion
    {
        self.rot
    }

    /// Samples the touch sensor and computes the deltas since the last sample.
    pub fn sample(&mut self)
    {
        let new = if let Some(saved) = *TOUCH.saved.rlock() {
            saved
        } else {
            self.saved = None;
            self.trans = Vector::default();
            self.rot = Quaternion::default();
            return;
        };
        let old = self.saved.unwrap_or(new);
        self.saved = Some(new);
        // Make sure that the points are in the same order as in the last poll by
        // verifying which are closest to which.
        let sqdist0 = old.0.sq_distance(new.0);
        let sqdist1 = old.0.sq_distance(new.1);
        let new = if sqdist0 <= sqdist1 {
            (new.0, new.1)
        } else {
            (new.1, new.0)
        };
        // Compute the pivot of the two touch point samples, which is the middle point
        // between their two respective touch points.
        let old_pivot = old.0.lerp(old.1, Scalar::from_val(0.5));
        let new_pivot = new.0.lerp(new.1, Scalar::from_val(0.5));
        // Compute the translation, which is just the difference between the pivots.
        self.trans = new_pivot - old_pivot;
        // Compute the rotation by calculating the angle between the vectors created by
        // the difference between the two contacts in each sample.
        let old = Normal::from_vec(old.1 - old.0);
        let new = Normal::from_vec(new.1 - new.0);
        self.rot = Quaternion::from_normals(old, new);
    }
}
