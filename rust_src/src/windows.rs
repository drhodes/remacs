//! Functions operating on windows.

use std::ptr;

use libc::c_int;

use remacs_macros::lisp_fn;

use crate::{
    editfns::{goto_char, point},
    frames::{LispFrameOrSelected, LispFrameRef},
    interactive::prefix_numeric_value,
    lisp::defsubr,
    lisp::{ExternalPtr, LispObject},
    lists::{assq, setcdr},
    marker::{marker_position_lisp, set_marker_restricted},
    remacs_sys::globals,
    remacs_sys::Fcopy_alist,
    remacs_sys::{
        estimate_mode_line_height, minibuf_level,
        minibuf_selected_window as current_minibuf_window, scroll_command, select_window,
        selected_window as current_window, set_buffer_internal, set_window_hscroll,
        update_mode_lines, window_body_width, window_list_1, window_menu_bar_p, window_tool_bar_p,
        wset_redisplay,
    },
    remacs_sys::{face_id, glyph_matrix, pvec_type, EmacsInt, Lisp_Type, Lisp_Window},
    remacs_sys::{
        Qceiling, Qfloor, Qheader_line_format, Qmode_line_format, Qnil, Qnone, Qwindow_live_p,
        Qwindow_valid_p, Qwindowp,
    },
    threads::ThreadState,
};

pub type LispWindowRef = ExternalPtr<Lisp_Window>;

impl LispWindowRef {
    /// Check if window is a live window (displays a buffer).
    /// This is also sometimes called a "leaf window" in Emacs sources.
    pub fn is_live(self) -> bool {
        self.contents.is_buffer()
    }

    pub fn is_pseudo(self) -> bool {
        self.pseudo_window_p()
    }

    /// A window of any sort, leaf or interior, is "valid" if its
    /// contents slot is non-nil.
    pub fn is_valid(self) -> bool {
        self.contents.is_not_nil()
    }

    /// Return the current height of the mode line of window W. If not known
    /// from W->mode_line_height, look at W's current glyph matrix, or return
    /// a default based on the height of the font of the face `mode-line'.
    pub fn current_mode_line_height(&mut self) -> i32 {
        let mode_line_height = self.mode_line_height;
        let matrix_mode_line_height =
            LispGlyphMatrixRef::new(self.current_matrix).mode_line_height();

        if mode_line_height >= 0 {
            mode_line_height
        } else if matrix_mode_line_height != 0 {
            self.mode_line_height = matrix_mode_line_height;
            matrix_mode_line_height
        } else {
            let mut frame = self.frame.as_frame_or_error();
            let window = selected_window().as_window_or_error();
            let mode_line_height = unsafe {
                estimate_mode_line_height(frame.as_mut(), CURRENT_MODE_LINE_FACE_ID(window))
            };
            self.mode_line_height = mode_line_height;
            mode_line_height
        }
    }

    pub fn start_marker(self) -> LispObject {
        self.start
    }

    pub fn is_internal(self) -> bool {
        self.contents.is_window()
    }

    pub fn is_minibuffer(self) -> bool {
        self.mini()
    }

    pub fn is_menu_bar(mut self) -> bool {
        unsafe { window_menu_bar_p(self.as_mut()) }
    }

    pub fn is_tool_bar(mut self) -> bool {
        unsafe { window_tool_bar_p(self.as_mut()) }
    }

    pub fn total_width(self, round: LispObject) -> i32 {
        let qfloor = Qfloor;
        let qceiling = Qceiling;

        if !(round == qfloor || round == qceiling) {
            self.total_cols
        } else {
            let frame = self.frame.as_frame_or_error();
            let unit = frame.column_width;

            if round == qceiling {
                (self.pixel_width + unit - 1) / unit
            } else {
                self.pixel_width / unit
            }
        }
    }

    pub fn total_height(self, round: LispObject) -> i32 {
        let qfloor = Qfloor;
        let qceiling = Qceiling;

        if !(round == qfloor || round == qceiling) {
            self.total_lines
        } else {
            let frame = self.frame.as_frame_or_error();
            let unit = frame.line_height;

            if round == qceiling {
                (self.pixel_height + unit - 1) / unit
            } else {
                self.pixel_height / unit
            }
        }
    }

    /// The frame x-position at which the text (or left fringe) in
    /// window starts. This does not include a left-hand scroll bar
    /// if any.
    pub fn left_edge_x(self) -> i32 {
        self.frame.as_frame_or_error().internal_border_width() + self.left_pixel_edge()
    }

    /// The frame y-position at which the window starts.
    pub fn top_edge_y(self) -> i32 {
        let mut y = self.top_pixel_edge();
        if !(self.is_menu_bar() || self.is_tool_bar()) {
            y += self.frame.as_frame_or_error().internal_border_width();
        }
        y
    }

    /// The pixel value where the text (or left fringe) in window starts.
    pub fn left_pixel_edge(self) -> i32 {
        self.pixel_left
    }

    /// The top pixel edge at which the window starts.
    /// This includes a header line, if any.
    pub fn top_pixel_edge(self) -> i32 {
        self.pixel_top
    }

    /// Convert window relative pixel Y to frame pixel coordinates.
    pub fn frame_pixel_y(self, y: i32) -> i32 {
        y + self.top_edge_y()
    }

    /// True if window wants a mode line and is high enough to
    /// accommodate it, false otherwise.
    ///
    /// Window wants a mode line if it's a leaf window and neither a minibuffer
    /// nor a pseudo window.  Moreover, its 'window-mode-line-format'
    /// parameter must not be 'none' and either that parameter or W's
    /// buffer's 'mode-line-format' value must be non-nil.  Finally, W must
    /// be higher than its frame's canonical character height.
    pub fn wants_mode_line(self) -> bool {
        let window_mode_line_format = self.get_parameter(Qmode_line_format);

        self.is_live()
            && !self.is_minibuffer()
            && !self.is_pseudo()
            && !window_mode_line_format.eq(Qnone)
            && (window_mode_line_format.is_not_nil()
                || self
                    .contents
                    .as_buffer_or_error()
                    .mode_line_format_
                    .is_not_nil())
            && self.pixel_height > self.frame.as_frame_or_error().line_height
    }

    /// True if window wants a header line and is high enough to
    /// accommodate it, false otherwise.
    ///
    /// Window wants a header line if it's a leaf window and neither a minibuffer
    /// nor a pseudo window.  Moreover, its 'window-header-line-format'
    /// parameter must not be 'none' and either that parameter or window's
    /// buffer's 'header-line-format' value must be non-nil.  Finally, window must
    /// be higher than its frame's canonical character height and be able to
    /// accommodate a mode line too if necessary (the mode line prevails).
    pub fn wants_header_line(self) -> bool {
        let window_header_line_format = self.get_parameter(Qheader_line_format);

        let mut height = self.frame.as_frame_or_error().line_height;
        if self.wants_mode_line() {
            height *= 2;
        }

        self.is_live()
            && !self.is_minibuffer()
            && !self.is_pseudo()
            && !window_header_line_format.eq(Qnone)
            && (window_header_line_format.is_not_nil()
                || (self.contents.as_buffer_or_error().header_line_format_).is_not_nil())
            && self.pixel_height > height
    }

    /// True if window W is a vertical combination of windows.
    pub fn is_vertical_combination(self) -> bool {
        self.is_internal() && !self.horizontal()
    }

    pub fn get_parameter(self, parameter: LispObject) -> LispObject {
        match assq(parameter, self.window_parameters).into() {
            Some((_, cdr)) => cdr,
            None => Qnil,
        }
    }
}

impl From<LispObject> for LispWindowRef {
    fn from(o: LispObject) -> Self {
        o.as_window().unwrap_or_else(|| wrong_type!(Qwindowp, o))
    }
}

impl From<LispWindowRef> for LispObject {
    fn from(w: LispWindowRef) -> Self {
        LispObject::tag_ptr(w, Lisp_Type::Lisp_Vectorlike)
    }
}

impl From<LispObject> for Option<LispWindowRef> {
    fn from(o: LispObject) -> Self {
        o.as_vectorlike().and_then(|v| v.as_window())
    }
}

impl LispObject {
    pub fn is_window(self) -> bool {
        self.as_vectorlike()
            .map_or(false, |v| v.is_pseudovector(pvec_type::PVEC_WINDOW))
    }

    pub fn as_window(self) -> Option<LispWindowRef> {
        self.into()
    }

    pub fn as_window_or_error(self) -> LispWindowRef {
        self.into()
    }

    pub fn as_minibuffer_or_error(self) -> LispWindowRef {
        let w = self
            .as_window()
            .unwrap_or_else(|| wrong_type!(Qwindowp, self));
        if !w.is_minibuffer() {
            error!("Window is not a minibuffer window");
        }
        w
    }

    pub fn as_live_window(self) -> Option<LispWindowRef> {
        self.as_window()
            .and_then(|w| if w.is_live() { Some(w) } else { None })
    }

    pub fn as_live_window_or_error(self) -> LispWindowRef {
        self.as_live_window()
            .unwrap_or_else(|| wrong_type!(Qwindow_live_p, self))
    }

    pub fn as_valid_window(self) -> Option<LispWindowRef> {
        self.as_window()
            .and_then(|w| if w.is_valid() { Some(w) } else { None })
    }

    pub fn as_valid_window_or_error(self) -> LispWindowRef {
        self.as_valid_window()
            .unwrap_or_else(|| wrong_type!(Qwindow_valid_p, self))
    }
}

pub type LispGlyphMatrixRef = ExternalPtr<glyph_matrix>;

impl LispGlyphMatrixRef {
    pub fn mode_line_height(self) -> i32 {
        if self.is_null() || self.rows.is_null() {
            0
        } else {
            unsafe { (*self.rows.offset((self.nrows - 1) as isize)).height }
        }
    }
}

pub struct LispWindowOrSelected(LispObject);

impl From<LispObject> for LispWindowOrSelected {
    fn from(obj: LispObject) -> LispWindowOrSelected {
        LispWindowOrSelected(obj.map_or_else(selected_window, |w| w))
    }
}

impl From<LispWindowOrSelected> for LispObject {
    fn from(w: LispWindowOrSelected) -> LispObject {
        w.0
    }
}

impl From<LispWindowOrSelected> for LispWindowRef {
    fn from(w: LispWindowOrSelected) -> LispWindowRef {
        w.0.as_window_or_error()
    }
}

pub struct LispWindowLiveOrSelected(LispWindowRef);

impl From<LispObject> for LispWindowLiveOrSelected {
    /// Same as the `decode_live_window` function
    fn from(obj: LispObject) -> LispWindowLiveOrSelected {
        LispWindowLiveOrSelected(obj.map_or_else(
            || selected_window().as_window_or_error(),
            |w| w.as_live_window_or_error(),
        ))
    }
}

impl From<LispWindowLiveOrSelected> for LispWindowRef {
    fn from(w: LispWindowLiveOrSelected) -> LispWindowRef {
        w.0
    }
}

pub struct LispWindowValidOrSelected(LispWindowRef);

impl From<LispObject> for LispWindowValidOrSelected {
    /// Same as the `decode_valid_window` function
    fn from(obj: LispObject) -> LispWindowValidOrSelected {
        LispWindowValidOrSelected(obj.map_or_else(
            || selected_window().as_window_or_error(),
            |w| w.as_valid_window_or_error(),
        ))
    }
}

impl From<LispWindowValidOrSelected> for LispWindowRef {
    fn from(w: LispWindowValidOrSelected) -> LispWindowRef {
        w.0
    }
}

#[no_mangle]
pub extern "C" fn decode_any_window(window: LispObject) -> LispWindowRef {
    LispWindowOrSelected::from(window).into()
}

/// Return t if OBJECT is a window and nil otherwise.
#[lisp_fn]
pub fn windowp(object: LispObject) -> bool {
    object.is_window()
}

/// Return t if OBJECT is a live window and nil otherwise.
///
/// A live window is a window that displays a buffer.
/// Internal windows and deleted windows are not live.
#[lisp_fn]
pub fn window_live_p(object: Option<LispWindowRef>) -> bool {
    object.map_or(false, |m| m.is_live())
}

/// Return current value of point in WINDOW.
/// WINDOW must be a live window and defaults to the selected one.
///
/// For a nonselected window, this is the value point would have if that
/// window were selected.
///
/// Note that, when WINDOW is selected, the value returned is the same as
/// that returned by `point' for WINDOW's buffer.  It would be more strictly
/// correct to return the top-level value of `point', outside of any
/// `save-excursion' forms.  But that is hard to define.
#[lisp_fn(min = "0")]
pub fn window_point(window: LispWindowLiveOrSelected) -> Option<EmacsInt> {
    let win: LispWindowRef = window.into();
    if win == selected_window().as_window_or_error() {
        Some(point())
    } else {
        marker_position_lisp(win.pointm.into())
    }
}

/// Return the selected window.
/// The selected window is the window in which the standard cursor for
/// selected windows appears and to which many commands apply.
#[lisp_fn]
pub fn selected_window() -> LispObject {
    unsafe { current_window }
}

/// Return the buffer displayed in window WINDOW.
/// If WINDOW is omitted or nil, it defaults to the selected window.
/// Return nil for an internal window or a deleted window.
#[lisp_fn(min = "0")]
pub fn window_buffer(window: LispWindowValidOrSelected) -> LispObject {
    let win: LispWindowRef = window.into();
    if win.is_live() {
        win.contents
    } else {
        Qnil
    }
}

/// Return t if OBJECT is a valid window and nil otherwise.
/// A valid window is either a window that displays a buffer or an internal
/// window.  Windows that have been deleted are not valid.
#[lisp_fn]
pub fn window_valid_p(object: Option<LispWindowRef>) -> bool {
    object.map_or(false, |w| w.is_valid())
}

/// Return position at which display currently starts in WINDOW.
/// WINDOW must be a live window and defaults to the selected one.
/// This is updated by redisplay or by calling `set-window-start'.
#[lisp_fn(min = "0")]
pub fn window_start(window: LispWindowLiveOrSelected) -> Option<EmacsInt> {
    let win: LispWindowRef = window.into();
    marker_position_lisp(win.start_marker().into())
}

/// Return non-nil if WINDOW is a minibuffer window.
/// WINDOW must be a valid window and defaults to the selected one.
#[lisp_fn(min = "0")]
pub fn window_minibuffer_p(window: LispWindowValidOrSelected) -> bool {
    let win: LispWindowRef = window.into();
    win.is_minibuffer()
}

/// Return the width of window WINDOW in pixels.
/// WINDOW must be a valid window and defaults to the selected one.
///
/// The return value includes the fringes and margins of WINDOW as well as
/// any vertical dividers or scroll bars belonging to WINDOW.  If WINDOW is
/// an internal window, its pixel width is the width of the screen areas
/// spanned by its children.
#[lisp_fn(min = "0")]
pub fn window_pixel_width(window: LispWindowValidOrSelected) -> i32 {
    let win: LispWindowRef = window.into();
    win.pixel_width
}

/// Return the height of window WINDOW in pixels.
/// WINDOW must be a valid window and defaults to the selected one.
///
/// The return value includes the mode line and header line and the bottom
/// divider, if any.  If WINDOW is an internal window, its pixel height is
/// the height of the screen areas spanned by its children.
#[lisp_fn(min = "0")]
pub fn window_pixel_height(window: LispWindowValidOrSelected) -> i32 {
    let win: LispWindowRef = window.into();
    win.pixel_height
}

/// Get width of marginal areas of window WINDOW.
/// WINDOW must be a live window and defaults to the selected one.
///
/// Value is a cons of the form (LEFT-WIDTH . RIGHT-WIDTH).
/// If a marginal area does not exist, its width will be returned
/// as nil.
#[lisp_fn(min = "0")]
pub fn window_margins(window: LispWindowLiveOrSelected) -> (LispObject, LispObject) {
    fn margin_as_object(margin: c_int) -> LispObject {
        if margin == 0 {
            Qnil
        } else {
            LispObject::from(margin)
        }
    }
    let win: LispWindowRef = window.into();

    (
        margin_as_object(win.left_margin_cols),
        margin_as_object(win.right_margin_cols),
    )
}

/// Return combination limit of window WINDOW.
/// WINDOW must be a valid window used in horizontal or vertical combination.
/// If the return value is nil, child windows of WINDOW can be recombined with
/// WINDOW's siblings.  A return value of t means that child windows of
/// WINDOW are never (re-)combined with WINDOW's siblings.
#[lisp_fn]
pub fn window_combination_limit(window: LispWindowRef) -> LispObject {
    if !window.is_internal() {
        error!("Combination limit is meaningful for internal windows only");
    }

    window.combination_limit
}

/// Set combination limit of window WINDOW to LIMIT; return LIMIT.
/// WINDOW must be a valid window used in horizontal or vertical combination.
/// If LIMIT is nil, child windows of WINDOW can be recombined with WINDOW's
/// siblings.  LIMIT t means that child windows of WINDOW are never
/// (re-)combined with WINDOW's siblings.  Other values are reserved for
/// future use.
#[lisp_fn]
pub fn set_window_combination_limit(mut window: LispWindowRef, limit: LispObject) -> LispObject {
    if !window.is_internal() {
        error!("Combination limit is meaningful for internal windows only");
    }

    window.combination_limit = limit;

    limit
}

/// Return the window selected just before minibuffer window was selected.
/// Return nil if the selected window is not a minibuffer window.
#[lisp_fn]
pub fn minibuffer_selected_window() -> LispObject {
    let level = unsafe { minibuf_level };
    let current_minibuf = unsafe { current_minibuf_window };
    if level > 0
        && selected_window().as_window_or_error().is_minibuffer()
        && current_minibuf.as_window().unwrap().is_live()
    {
        current_minibuf
    } else {
        Qnil
    }
}

/// Return the total width of window WINDOW in columns.
/// WINDOW is optional and defaults to the selected window. If provided it must
/// be a valid window.
///
/// The return value includes the widths of WINDOW's fringes, margins,
/// scroll bars and its right divider, if any.  If WINDOW is an internal
/// window, the total width is the width of the screen areas spanned by its
/// children.
///
/// If WINDOW's pixel width is not an integral multiple of its frame's
/// character width, the number of lines occupied by WINDOW is rounded
/// internally.  This is done in a way such that, if WINDOW is a parent
/// window, the sum of the total widths of all its children internally
/// equals the total width of WINDOW.
///
/// If the optional argument ROUND is `ceiling', return the smallest integer
/// larger than WINDOW's pixel width divided by the character width of
/// WINDOW's frame.  ROUND `floor' means to return the largest integer
/// smaller than WINDOW's pixel width divided by the character width of
/// WINDOW's frame.  Any other value of ROUND means to return the internal
/// total width of WINDOW.
#[lisp_fn(min = "0")]
pub fn window_total_width(window: LispWindowValidOrSelected, round: LispObject) -> i32 {
    let win: LispWindowRef = window.into();
    win.total_width(round)
}

/// Return the height of window WINDOW in lines.
/// WINDOW is optional and defaults to the selected window. If provided it must
/// be a valid window.
///
/// The return value includes the heights of WINDOW's mode and header line
/// and its bottom divider, if any.  If WINDOW is an internal window, the
/// total height is the height of the screen areas spanned by its children.
///
/// If WINDOW's pixel height is not an integral multiple of its frame's
/// character height, the number of lines occupied by WINDOW is rounded
/// internally.  This is done in a way such that, if WINDOW is a parent
/// window, the sum of the total heights of all its children internally
/// equals the total height of WINDOW.
///
/// If the optional argument ROUND is `ceiling', return the smallest integer
/// larger than WINDOW's pixel height divided by the character height of
/// WINDOW's frame.  ROUND `floor' means to return the largest integer
/// smaller than WINDOW's pixel height divided by the character height of
/// WINDOW's frame.  Any other value of ROUND means to return the internal
/// total height of WINDOW.
#[lisp_fn(min = "0")]
pub fn window_total_height(window: LispWindowValidOrSelected, round: LispObject) -> i32 {
    let win: LispWindowRef = window.into();
    win.total_height(round)
}

/// Return the parent window of window WINDOW.
/// WINDOW must be a valid window and defaults to the selected one.
/// Return nil for a window with no parent (e.g. a root window).
#[lisp_fn(min = "0")]
pub fn window_parent(window: LispWindowValidOrSelected) -> LispObject {
    let win: LispWindowRef = window.into();
    win.parent
}

/// Return the frame that window WINDOW is on.
/// WINDOW is optional and defaults to the selected window. If provided it must
/// be a valid window.
#[lisp_fn(min = "0")]
pub fn window_frame(window: LispWindowValidOrSelected) -> LispObject {
    let win: LispWindowRef = window.into();
    win.frame
}

/// Return the minibuffer window for frame FRAME.
/// If FRAME is omitted or nil, it defaults to the selected frame.
#[lisp_fn(min = "0")]
pub fn minibuffer_window(frame: LispFrameOrSelected) -> LispObject {
    let frame = frame.live_or_error();
    frame.minibuffer_window
}

/// Return WINDOW's value for PARAMETER.
/// WINDOW can be any window and defaults to the selected one.
#[lisp_fn(name = "window-parameter", c_name = "window_parameter")]
pub fn window_parameter_lisp(window: LispWindowOrSelected, parameter: LispObject) -> LispObject {
    let win: LispWindowRef = window.into();
    win.get_parameter(parameter)
}

/// Return the display-table that WINDOW is using.
/// WINDOW must be a live window and defaults to the selected one.
#[lisp_fn(min = "0")]
pub fn window_display_table(window: LispWindowLiveOrSelected) -> LispObject {
    let win: LispWindowRef = window.into();
    win.display_table
}

/// Set WINDOW's display-table to TABLE.
/// WINDOW must be a live window and defaults to the selected one.
#[lisp_fn]
pub fn set_window_display_table(window: LispWindowLiveOrSelected, table: LispObject) -> LispObject {
    let mut win: LispWindowRef = window.into();
    win.display_table = table;
    table
}

pub fn window_wants_mode_line(window: LispWindowRef) -> bool {
    window.wants_mode_line()
}

pub fn window_wants_header_line(window: LispWindowRef) -> bool {
    window.wants_header_line()
}

/// Set WINDOW's value of PARAMETER to VALUE.
/// WINDOW can be any window and defaults to the selected one.
/// Return VALUE.
#[lisp_fn]
pub fn set_window_parameter(
    window: LispWindowOrSelected,
    parameter: LispObject,
    value: LispObject,
) -> LispObject {
    let mut win: LispWindowRef = window.into();
    let old_alist_elt = assq(parameter, win.window_parameters);
    if old_alist_elt.is_nil() {
        win.window_parameters = ((parameter, value), win.window_parameters).into();
    } else {
        setcdr(old_alist_elt.into(), value);
    }
    value
}

/// Return the desired face id for the mode line of a window, depending
/// on whether the window is selected or not, or if the window is the
/// scrolling window for the currently active minibuffer window.
///
/// Due to the way display_mode_lines manipulates with the contents of
/// selected_window, this function needs three arguments: SELW which is
/// compared against the current value of selected_window, MBW which is
/// compared against minibuf_window (if SELW doesn't match), and SCRW
/// which is compared against minibuf_selected_window (if MBW matches).
#[no_mangle]
pub extern "C" fn CURRENT_MODE_LINE_FACE_ID_3(
    selw: LispWindowRef,
    mbw: LispWindowRef,
    scrw: LispWindowRef,
) -> face_id {
    let current = if let Some(w) = selected_window().as_window() {
        w
    } else {
        LispWindowRef::new(ptr::null_mut())
    };

    unsafe {
        if !globals.mode_line_in_non_selected_windows || selw == current {
            return face_id::MODE_LINE_FACE_ID;
        } else if minibuf_level > 0 {
            if let Some(minibuf_window) = current_minibuf_window.as_window() {
                if mbw == minibuf_window && scrw == minibuf_window {
                    return face_id::MODE_LINE_FACE_ID;
                }
            }
        }

        face_id::MODE_LINE_INACTIVE_FACE_ID
    }
}

/// Return the desired face id for the mode line of window W.
#[no_mangle]
pub extern "C" fn CURRENT_MODE_LINE_FACE_ID(window: LispWindowRef) -> face_id {
    let current = if let Some(w) = selected_window().as_window() {
        w
    } else {
        LispWindowRef::new(ptr::null_mut())
    };

    CURRENT_MODE_LINE_FACE_ID_3(window, current, window)
}

#[no_mangle]
pub extern "C" fn CURRENT_MODE_LINE_HEIGHT(mut window: LispWindowRef) -> i32 {
    window.current_mode_line_height()
}

/// Return a list of windows on FRAME, starting with WINDOW.
/// FRAME nil or omitted means use the selected frame.
/// WINDOW nil or omitted means use the window selected within FRAME.
/// MINIBUF t means include the minibuffer window, even if it isn't active.
/// MINIBUF nil or omitted means include the minibuffer window only
/// if it's active.
/// MINIBUF neither nil nor t means never include the minibuffer window.
#[lisp_fn(min = "0")]
pub fn window_list(
    frame: LispFrameOrSelected,
    minibuf: LispObject,
    window: Option<LispWindowRef>,
) -> LispObject {
    let w_obj = match window {
        Some(w) => w.into(),
        None => LispFrameRef::from(frame).selected_window,
    };

    let w_ref = w_obj
        .as_window()
        .unwrap_or_else(|| panic!("Invalid window reference."));

    let f_obj = LispObject::from(frame);

    if !f_obj.eq(w_ref.frame) {
        error!("Window is on a different frame");
    }

    unsafe { (window_list_1(w_obj, minibuf, f_obj)) }
}

/// Return a list of all live windows.
/// WINDOW specifies the first window to list and defaults to the selected
/// window.
///
/// Optional argument MINIBUF nil or omitted means consider the minibuffer
/// window only if the minibuffer is active.  MINIBUF t means consider the
/// minibuffer window even if the minibuffer is not active.  Any other value
/// means do not consider the minibuffer window even if the minibuffer is
/// active.
///
/// Optional argument ALL-FRAMES nil or omitted means consider all windows
/// on WINDOW's frame, plus the minibuffer window if specified by the
/// MINIBUF argument.  If the minibuffer counts, consider all windows on all
/// frames that share that minibuffer too.  The following non-nil values of
/// ALL-FRAMES have special meanings:
///
/// - t means consider all windows on all existing frames.
///
/// - `visible' means consider all windows on all visible frames.
///
/// - 0 (the number zero) means consider all windows on all visible and
///   iconified frames.
///
/// - A frame means consider all windows on that frame only.
///
/// Anything else means consider all windows on WINDOW's frame and no
/// others.
///
/// If WINDOW is not on the list of windows returned, some other window will
/// be listed first but no error is signaled.
#[lisp_fn(min = "0", name = "window-list-1", c_name = "window_list_1")]
pub fn window_list_1_lisp(
    window: LispObject,
    minibuf: LispObject,
    all_frames: LispObject,
) -> LispObject {
    unsafe { (window_list_1(window, minibuf, all_frames)) }
}

/// Return non-nil when WINDOW is dedicated to its buffer.
/// More precisely, return the value assigned by the last call of
/// `set-window-dedicated-p' for WINDOW.  Return nil if that function was
/// never called with WINDOW as its argument, or the value set by that
/// function was internally reset since its last call.  WINDOW must be a
/// live window and defaults to the selected one.
///
/// When a window is dedicated to its buffer, `display-buffer' will refrain
/// from displaying another buffer in it.  `get-lru-window' and
/// `get-largest-window' treat dedicated windows specially.
/// `delete-windows-on', `replace-buffer-in-windows', `quit-window' and
/// `kill-buffer' can delete a dedicated window and the containing frame.
///
/// Functions like `set-window-buffer' may change the buffer displayed by a
/// window, unless that window is "strongly" dedicated to its buffer, that
/// is the value returned by `window-dedicated-p' is t.
#[lisp_fn(min = "0")]
pub fn window_dedicated_p(window: LispWindowOrSelected) -> LispObject {
    let win: LispWindowRef = window.into();
    win.dedicated
}

/// Mark WINDOW as dedicated according to FLAG.
/// WINDOW must be a live window and defaults to the selected one.  FLAG
/// non-nil means mark WINDOW as dedicated to its buffer.  FLAG nil means
/// mark WINDOW as non-dedicated.  Return FLAG.
///
/// When a window is dedicated to its buffer, `display-buffer' will refrain
/// from displaying another buffer in it.  `get-lru-window' and
/// `get-largest-window' treat dedicated windows specially.
/// `delete-windows-on', `replace-buffer-in-windows', `quit-window',
/// `quit-restore-window' and `kill-buffer' can delete a dedicated window
/// and the containing frame.
///
/// As a special case, if FLAG is t, mark WINDOW as "strongly" dedicated to
/// its buffer.  Functions like `set-window-buffer' may change the buffer
/// displayed by a window, unless that window is strongly dedicated to its
/// buffer.  If and when `set-window-buffer' displays another buffer in a
/// window, it also makes sure that the window is no more dedicated.
#[lisp_fn]
pub fn set_window_dedicated_p(window: LispWindowOrSelected, flag: LispObject) -> LispObject {
    let mut win: LispWindowRef = window.into();
    win.dedicated = flag;
    flag
}

/// Return old value of point in WINDOW.
/// WINDOW must be a live window and defaults to the selected one.
#[lisp_fn(min = "0")]
pub fn window_old_point(window: LispWindowLiveOrSelected) -> Option<EmacsInt> {
    let win: LispWindowRef = window.into();
    marker_position_lisp(win.old_pointm.into())
}

/// Return the use time of window WINDOW.
/// WINDOW must be a live window and defaults to the selected one. The
/// window with the highest use time is the most recently selected
/// one.  The window with the lowest use time is the least recently
/// selected one.
#[lisp_fn(min = "0")]
pub fn window_use_time(window: LispWindowLiveOrSelected) -> EmacsInt {
    let win: LispWindowRef = window.into();
    win.use_time
}

/// Return buffers previously shown in WINDOW.
/// WINDOW must be a live window and defaults to the selected one.
#[lisp_fn(min = "0")]
pub fn window_prev_buffers(window: LispWindowLiveOrSelected) -> LispObject {
    let win: LispWindowRef = window.into();
    win.prev_buffers
}

/// Set WINDOW's previous buffers to PREV-BUFFERS.
/// WINDOW must be a live window and defaults to the selected one.
/// PREV-BUFFERS should be a list of elements (BUFFER WINDOW-START POS),
/// where BUFFER is a buffer, WINDOW-START is the start position of the
/// window for that buffer, and POS is a window-specific point value.
#[lisp_fn]
pub fn set_window_prev_buffers(
    window: LispWindowLiveOrSelected,
    prev_buffers: LispObject,
) -> LispObject {
    let mut win: LispWindowRef = window.into();
    win.prev_buffers = prev_buffers;
    prev_buffers
}

/// Return list of buffers recently re-shown in WINDOW.
/// WINDOW must be a live window and defaults to the selected one.
#[lisp_fn(min = "0")]
pub fn window_next_buffers(window: LispWindowLiveOrSelected) -> LispObject {
    let win: LispWindowRef = window.into();
    win.next_buffers
}

/// Set WINDOW's next buffers to NEXT-BUFFERS.
/// WINDOW must be a live window and defaults to the selected one.
/// NEXT-BUFFERS should be a list of buffers.
#[lisp_fn]
pub fn set_window_next_buffers(
    window: LispWindowLiveOrSelected,
    next_buffers: LispObject,
) -> LispObject {
    let mut win: LispWindowRef = window.into();
    win.next_buffers = next_buffers;
    next_buffers
}

/// Make point value in WINDOW be at position POS in WINDOW's buffer.
/// WINDOW must be a live window and defaults to the selected one.
/// Return POS.
#[lisp_fn]
pub fn set_window_point(window: LispWindowLiveOrSelected, pos: LispObject) -> LispObject {
    let mut win: LispWindowRef = window.into();

    // Type of POS is checked by Fgoto_char or set_marker_restricted ...
    if win == selected_window().as_window_or_error() {
        let mut current_buffer = ThreadState::current_buffer_unchecked();

        if win
            .contents
            .as_buffer()
            .map_or(false, |b| b == current_buffer)
        {
            goto_char(pos);
        } else {
            // ... but here we want to catch type error before buffer change.
            pos.as_number_coerce_marker_or_error();
            unsafe {
                set_buffer_internal(win.contents.as_buffer_or_error().as_mut());
            }
            goto_char(pos);
            unsafe {
                set_buffer_internal(current_buffer.as_mut());
            }
        }
    } else {
        set_marker_restricted(win.pointm, pos, win.contents);
        // We have to make sure that redisplay updates the window to show
        // the new value of point.
        win.set_redisplay(true);
    }
    pos
}

/// Make display in WINDOW start at position POS in WINDOW's buffer.
/// WINDOW must be a live window and defaults to the selected one.  Return
/// POS.  Optional third arg NOFORCE non-nil inhibits next redisplay from
/// overriding motion of point in order to display at this exact start.
#[lisp_fn(min = "2")]
pub fn set_window_start(
    window: LispWindowLiveOrSelected,
    pos: LispObject,
    noforce: bool,
) -> LispObject {
    let mut win: LispWindowRef = window.into();
    set_marker_restricted(win.start, pos, win.contents);
    // This is not right, but much easier than doing what is right.
    win.set_start_at_line_beg(false);
    if !noforce {
        win.set_force_start(true);
    }

    wset_update_mode_line(win);

    // Bug#15957
    win.set_window_end_valid(false);
    unsafe { wset_redisplay(win.as_mut()) };
    pos
}

/// Return the topmost child window of window WINDOW.
/// WINDOW must be a valid window and defaults to the selected one.
/// Return nil if WINDOW is a live window (live windows have no children).
/// Return nil if WINDOW is an internal window whose children form a
/// horizontal combination.
#[lisp_fn(min = "0")]
pub fn window_top_child(window: LispWindowValidOrSelected) -> Option<LispWindowRef> {
    let win: LispWindowRef = window.into();
    if win.is_vertical_combination() {
        win.contents.as_window()
    } else {
        None
    }
}

pub fn scroll_horizontally(arg: LispObject, set_minimum: LispObject, left: bool) -> LispObject {
    let mut w = selected_window().as_window_or_error();
    let requested_arg = if arg.is_nil() {
        unsafe { EmacsInt::from(window_body_width(w.as_mut(), false)) - 2 }
    } else if left {
        prefix_numeric_value(arg)
    } else {
        -prefix_numeric_value(arg)
    };

    let result = unsafe { set_window_hscroll(w.as_mut(), w.hscroll as EmacsInt + requested_arg) };

    if set_minimum.is_not_nil() {
        w.min_hscroll = w.hscroll;
    }

    w.set_suspend_auto_hscroll(true);
    result
}

/// Scroll selected window display ARG columns left.
/// Default for ARG is window width minus 2.
/// Value is the total amount of leftward horizontal scrolling in
/// effect after the change.
/// If SET-MINIMUM is non-nil, the new scroll amount becomes the
/// lower bound for automatic scrolling, i.e. automatic scrolling
/// will not scroll a window to a column less than the value returned
/// by this function.  This happens in an interactive call.
#[lisp_fn(min = "0", intspec = "^P\np")]
pub fn scroll_left(arg: LispObject, set_minimum: LispObject) -> LispObject {
    scroll_horizontally(arg, set_minimum, true)
}

/// Scroll selected window display ARG columns left.
/// Default for ARG is window width minus 2.
/// Value is the total amount of leftward horizontal scrolling in
/// effect after the change.
/// If SET-MINIMUM is non-nil, the new scroll amount becomes the
/// lower bound for automatic scrolling, i.e. automatic scrolling
/// will not scroll a window to a column less than the value returned
/// by this function.  This happens in an interactive call.
#[lisp_fn(min = "0", intspec = "^P\np")]
pub fn scroll_right(arg: LispObject, set_minimum: LispObject) -> LispObject {
    scroll_horizontally(arg, set_minimum, false)
}

/// Scroll text of selected window upward ARG lines.
/// If ARG is omitted or nil, scroll upward by a near full screen.
/// A near full screen is `next-screen-context-lines' less than a full screen.
/// Negative ARG means scroll downward.
/// If ARG is the atom `-', scroll downward by nearly full screen.
/// When calling from a program, supply as argument a number, nil, or `-'.
#[lisp_fn(min = "0", intspec = "^P")]
pub fn scroll_up(arg: LispObject) {
    unsafe { scroll_command(arg, 1) };
}

/// Scroll text of selected window down ARG lines.
/// If ARG is omitted or nil, scroll down by a near full screen.
/// A near full screen is `next-screen-context-lines' less than a full screen.
/// Negative ARG means scroll upward.
/// If ARG is the atom `-', scroll upward by nearly full screen.
/// When calling from a program, supply as argument a number, nil, or `-'.
#[lisp_fn(min = "0", intspec = "^P")]
pub fn scroll_down(arg: LispObject) {
    unsafe { scroll_command(arg, -1) };
}

/// Return new normal size of window WINDOW.
/// WINDOW must be a valid window and defaults to the selected one.
///
/// The new normal size of WINDOW is the value set by the last call of
/// `set-window-new-normal' for WINDOW.  If valid, it will be shortly
/// installed as WINDOW's normal size (see `window-normal-size').
#[lisp_fn(min = "0")]
pub fn window_new_normal(window: LispWindowValidOrSelected) -> LispObject {
    let win: LispWindowRef = window.into();
    win.new_normal
}

/// Return the new total size of window WINDOW.
/// WINDOW must be a valid window and defaults to the selected one.
///
/// The new total size of WINDOW is the value set by the last call of
/// `set-window-new-total' for WINDOW.  If it is valid, it will be shortly
/// installed as WINDOW's total height (see `window-total-height') or total
/// width (see `window-total-width').
#[lisp_fn(min = "0")]
pub fn window_new_total(window: LispWindowValidOrSelected) -> LispObject {
    let win: LispWindowRef = window.into();
    win.new_total
}

/// Set new total size of WINDOW to SIZE.
/// WINDOW must be a valid window and defaults to the selected one.
/// Return SIZE.
///
/// Optional argument ADD non-nil means add SIZE to the new total size of
/// WINDOW and return the sum.
///
/// The new total size of WINDOW, if valid, will be shortly installed as
/// WINDOW's total height (see `window-total-height') or total width (see
/// `window-total-width').
///
/// Note: This function does not operate on any child windows of WINDOW.
#[lisp_fn(min = "2")]
pub fn set_window_new_total(
    window: LispWindowValidOrSelected,
    size: EmacsInt,
    add: bool,
) -> LispObject {
    let mut win: LispWindowRef = window.into();

    let new_total = if !add {
        size
    } else {
        EmacsInt::from(win.new_total) + size
    };
    win.new_total = new_total.into();
    win.new_total
}

#[no_mangle]
pub extern "C" fn wset_update_mode_line(mut w: LispWindowRef) {
    // If this window is the selected window on its frame, set the
    // global variable update_mode_lines, so that x_consider_frame_title
    // will consider this frame's title for redisplay.
    let fselected_window = w.frame.as_frame_or_error().selected_window;

    if let Some(win) = fselected_window.as_window() {
        if win == w {
            unsafe {
                update_mode_lines = 42;
            }
        }
    } else {
        w.set_update_mode_line(true);
    }
}

/// Return the number of columns by which WINDOW is scrolled from left margin.
/// WINDOW must be a live window and defaults to the selected one.
#[lisp_fn(min = "0")]
pub fn window_hscroll(window: LispWindowLiveOrSelected) -> EmacsInt {
    let win: LispWindowRef = window.into();
    win.hscroll as EmacsInt
}

#[no_mangle]
pub extern "C" fn window_parameter(w: LispWindowRef, parameter: LispObject) -> LispObject {
    w.get_parameter(parameter)
}

/// Select WINDOW which must be a live window.
/// Also make WINDOW's frame the selected frame and WINDOW that frame's
/// selected window.  In addition, make WINDOW's buffer current and set its
/// buffer's value of `point' to the value of WINDOW's `window-point'.
/// Return WINDOW.
///
/// Optional second arg NORECORD non-nil means do not put this buffer at the
/// front of the buffer list and do not make this window the most recently
/// selected one.  Also, do not mark WINDOW for redisplay unless NORECORD
/// equals the special symbol `mark-for-redisplay'.
///
/// Run `buffer-list-update-hook' unless NORECORD is non-nil.  Note that
/// applications and internal routines often select a window temporarily for
/// various purposes; mostly, to simplify coding.  As a rule, such
/// selections should be not recorded and therefore will not pollute
/// `buffer-list-update-hook'.  Selections that "really count" are those
/// causing a visible change in the next redisplay of WINDOW's frame and
/// should be always recorded.  So if you think of running a function each
/// time a window gets selected put it on `buffer-list-update-hook'.
///
/// Also note that the main editor command loop sets the current buffer to
/// the buffer of the selected window before each command.
#[lisp_fn(min = "1", name = "select-window", c_name = "select_window")]
pub fn select_window_lisp(window: LispObject, norecord: LispObject) -> LispObject {
    unsafe { select_window(window, norecord, false) }
}

/// Return top line of window WINDOW.
/// This is the distance, in lines, between the top of WINDOW and the top
/// of the frame's window area.  For instance, the return value is 0 if
/// there is no window above WINDOW.
///
/// WINDOW must be a valid window and defaults to the selected one.
#[lisp_fn(min = "0")]
pub fn window_top_line(window: LispWindowValidOrSelected) -> EmacsInt {
    let win: LispWindowRef = window.into();
    EmacsInt::from(win.top_line)
}

/// Return the parameters of WINDOW and their values.
/// WINDOW must be a valid window and defaults to the selected one.  The
/// return value is a list of elements of the form (PARAMETER . VALUE).
#[lisp_fn(min = "0")]
pub fn window_parameters(window: LispWindowValidOrSelected) -> LispObject {
    let win: LispWindowRef = window.into();
    unsafe { Fcopy_alist(win.window_parameters) }
}

/// Return WINDOW's redisplay end trigger value.
/// WINDOW must be a live window and defaults to the selected one.
/// See `set-window-redisplay-end-trigger' for more information.
#[lisp_fn(min = "0")]
pub fn window_redisplay_end_trigger(window: LispWindowLiveOrSelected) -> LispObject {
    let win: LispWindowRef = window.into();
    win.redisplay_end_trigger
}

/// Set WINDOW's redisplay end trigger value to VALUE.
/// WINDOW must be a live window and defaults to the selected one.  VALUE
/// should be a buffer position (typically a marker) or nil.  If it is a
/// buffer position, then if redisplay in WINDOW reaches a position beyond
/// VALUE, the functions in `redisplay-end-trigger-functions' are called
/// with two arguments: WINDOW, and the end trigger value.  Afterwards the
/// end-trigger value is reset to nil.
#[lisp_fn]
pub fn set_window_redisplay_end_trigger(
    window: LispWindowLiveOrSelected,
    value: LispObject,
) -> LispObject {
    let mut win: LispWindowRef = window.into();
    win.redisplay_end_trigger = value;
    value
}

include!(concat!(env!("OUT_DIR"), "/windows_exports.rs"));
