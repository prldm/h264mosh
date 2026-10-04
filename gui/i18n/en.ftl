# English UI text; the fallback for every other language.
# Keys are referenced from the code as tr!("key"); see src/i18n.rs.
# Frame numbers are 0-based, in decode order, like on the command line.

app-title = h264mosh
window-title = { $project } – h264mosh
window-title-unsaved = • { $project } – h264mosh
empty-hint = Open one or more clips or a project to start, or drop them here.
open-button = Open…
open-tooltip = Open a project, or video clips: they are re-encoded with datamosh-friendly settings; a raw .264 stream opens as it is.

## Menus

menu-file = File
menu-open = Open…
menu-recent = Open recent
menu-save = Save
menu-save-as = Save as…
menu-export = Export…
menu-edit = Edit
menu-undo = Undo
menu-redo = Redo
menu-drop-keyframes = Drop keyframes
menu-help = Help
menu-shortcuts = Keyboard shortcuts
filter-all = Projects and videos
filter-project = h264mosh project
filter-video = Video
filter-mp4 = MP4 video
filter-raw = Raw H.264 stream

## Header

changes-pending =
    { $count ->
        [one] 1 change not rendered
       *[other] { $count } changes not rendered
    }
all-rendered = Rendered
render-button = Render
render-tooltip = Apply the pending changes (R). Deleted frames are removed; the preview shows the result.
auto-render = Auto
auto-render-tooltip = Render automatically after each change.
export-button = Export…
export-tooltip = Save the last render as a video file or as a raw H.264 stream.
export-pending-tooltip = Render the pending changes first.

## Background jobs and messages

cancel-button = Cancel
stage-starting = Starting…
stage-encoding = Encoding clip { $clip } of { $clips }
stage-reading = Reading the stream…
stage-rendering = Rendering…
stage-exporting = Exporting…
dismiss = Dismiss
import-failed = Could not open it: { $reason }
save-failed = Could not save the project: { $reason }
render-failed = Rendering failed: { $reason }
render-warning =
    { $count ->
        [one] Warning: { $text }
       *[other] Warning: { $text } (and { NUMBER($count, useGrouping: "false") } more)
    }
export-failed = Export failed: { $reason }
export-done = Exported to { $path }
preview-failed = The preview could not be decoded: { $reason }
error-first-frame-idr = The first frame must be a keyframe (IDR), and frame { NUMBER($frame, useGrouping: "false") } is not one. Restore a keyframe or move one to the start.
error-nothing-to-render = Every frame is deleted; there is nothing to render.
no-keyframes = There are no keyframes to drop.
unsaved-title = Unsaved changes
unsaved-text = Save the changes to { $project }?
unsaved-save = Save
unsaved-discard = Don't save

## Timeline

delete-button = Delete
delete-tooltip = Delete the selected frames (Del). They stay as empty slots until the next render.
restore-button = Restore
restore-tooltip = Bring back the selected deleted frames.
duplicate-button = Duplicate
duplicate-tooltip = Show each selected frame once more (B). Scroll over a frame's ×N badge, or hold B and scroll, to change the count.
unduplicate-button = One less
unduplicate-tooltip = Show each selected frame once less (Shift+B).
selection-count =
    { $count ->
        [one] 1 frame selected
       *[other] { $count } frames selected
    }
zoom-label = Zoom
zoom-tooltip = Ctrl + mouse wheel over the timeline zooms too.
copies-badge = ×{ $count }
frame-number = { NUMBER($n, useGrouping: "false") }
frame-range = { NUMBER($from, useGrouping: "false") }–{ NUMBER($to, useGrouping: "false") }
list-separator = {", "}
frame-badge-i = I
frame-badge-p = P
frame-badge-b = B
frame-badge-unknown = ?
frame-type-i = I-frame (keyframe)
frame-type-p = P-frame
frame-type-b = B-frame
frame-type-unknown = unknown frame type
card-tooltip = Source frame { NUMBER($frame, useGrouping: "false") } · { $kind }
card-tooltip-copies = Shown { $count } times
card-tooltip-deleted = Deleted: removed at the next render
card-tooltip-pending = Changed, not rendered yet
card-tooltip-removed = Deleted here: { $frames }

## Effects

effects-title = Effects
effects-none = No effects yet. Select frames and add one.
effects-need-cavlc = Effects need a CAVLC stream. Open the clips from video files (they are re-encoded so) instead of a raw stream.
effects-select-first = Select the frames first.
effect-add = Add effect
effect-label = { $name } { $summary }
effect-enabled-tooltip = On or bypassed
effect-up = Apply earlier
effect-down = Apply later
effect-used =
    { $count ->
        [one] On 1 frame
       *[other] On { $count } frames
    }
effect-idle =
    { $count ->
        [one] 1 of them is a keyframe, which this effect does not change.
       *[other] { $count } of them are keyframes, which this effect does not change.
    }
effect-apply = Add to selection
effect-apply-tooltip = Give this effect to the selected frames too.
effect-remove = Remove from selection
effect-remove-tooltip = Take this effect away from the selected frames.
effect-delete = Delete effect
effect-mv-scale = Motion scale
effect-mv-scale-about = Multiplies the motion vectors: movement gets stronger, reversed or frozen. P-frames only.
effect-mv-shift = Motion shift
effect-mv-shift-about = Adds an offset to every motion vector, so everything slides. P-frames only.
effect-mv-jitter = Motion jitter
effect-mv-jitter-about = A random offset for each macroblock. P-frames only.
effect-mvd-scale = Vector error
effect-mvd-scale-about = Multiplies the coded vector differences; the error spreads through the prediction, more chaotic than motion scale.
effect-kill-intra = Freeze
effect-kill-intra-about = Intra macroblocks of P-frames are skipped, so moshed areas never heal.
effect-zero-residual = Ghost
effect-zero-residual-about = Drops all residual data: pictures come from prediction only, no new detail comes in.
effect-debug = Debug effect
summary-scale = ×{ NUMBER($x, maximumFractionDigits: 2) } / ×{ NUMBER($y, maximumFractionDigits: 2) }
summary-shift = { NUMBER($x, maximumFractionDigits: 2) } / { NUMBER($y, maximumFractionDigits: 2) } px
summary-jitter = ±{ NUMBER($px, maximumFractionDigits: 2) } px
param-horizontal = Horizontal
param-vertical = Vertical
param-horizontal-px = Horizontal (px)
param-vertical-px = Vertical (px)
param-amount-px = Amount (px)
param-pattern = Pattern {" "}
param-new-pattern = New pattern

## Preview

first-frame = First frame (Home)
previous-frame = Previous frame (←)
next-frame = Next frame (→)
last-frame = Last frame (End)
play-tooltip = Play or pause (Space). J and L play backwards and forwards, faster on each press; K stops.
play-speed = { NUMBER($speed, maximumFractionDigits: 1) }×
preview-position = Frame { NUMBER($frame, useGrouping: "false") } of { $total }
preview-source = source frame { NUMBER($frame, useGrouping: "false") }
original-button = Original
original-tooltip = Hold to see the frame without the mosh (or hold \).
original-label = Original

## Export

export-title = Export
export-baked = Video, mosh baked in
export-baked-about = Decoded and encoded again: plays, seeks and edits like any video. This is what the preview shows.
export-quality = Quality
quality-high = High
quality-lossless = Lossless (large, for further editing)
quality-small = Smaller file
export-copy = Video, moshed stream as it is
export-copy-about = The moshed stream in an MP4 without re-encoding. Players decode the mosh themselves; seeking and editors may struggle.
export-raw = Raw H.264 stream (.264)
export-raw-about = The moshed stream itself, bit for bit.
export-choose-file = Choose file…

## Keyboard shortcuts

shortcuts-title = Keyboard shortcuts
shortcut-play = Play / pause
shortcut-shuttle = Play backwards, stop, play forwards (faster on each press)
shortcut-step = Previous / next frame, first / last frame
shortcut-original = Hold to see the original
shortcut-delete = Delete the selected frames
shortcut-drop-keyframes = Drop keyframes (selected, or all but the first)
shortcut-bloom = One copy more / less; hold B and scroll to change the count
shortcut-render = Render
shortcut-deselect = Clear the selection
shortcut-select-all = Select all
