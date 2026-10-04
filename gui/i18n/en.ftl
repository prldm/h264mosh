# English UI text; the fallback for every other language.
# Keys are referenced from the code as tr!("key"); see src/i18n.rs.
# Frame numbers are 0-based, in decode order, like on the command line.

app-title = h264mosh
window-title = { $project } – h264mosh
empty-hint = Open one or more clips to start, or drop them here.

## Header

open-button = Open…
open-tooltip = Open video clips. They are re-encoded with datamosh-friendly settings; a raw .264 stream opens as it is.
filter-video = Video
filter-mp4 = MP4 video
filter-raw = Raw H.264 stream
changes-pending =
    { $count ->
        [one] 1 change not rendered
       *[other] { $count } changes not rendered
    }
all-rendered = Rendered
render-button = Render
render-tooltip = Apply the pending changes. Deleted frames are removed; the preview shows the result.
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
import-failed = Could not open the clips: { $reason }
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

## Timeline

delete-button = Delete
delete-tooltip = Delete the selected frames (Del). They stay as empty slots until the next render.
restore-button = Restore
restore-tooltip = Bring back the selected deleted frames.
duplicate-button = Duplicate
duplicate-tooltip = Show each selected frame once more. Scroll over a frame's ×N badge to change the count.
unduplicate-button = One less
unduplicate-tooltip = Show each selected frame once less.
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

## Preview

first-frame = First frame (Home)
previous-frame = Previous frame (←)
next-frame = Next frame (→)
last-frame = Last frame (End)
preview-position = Frame { NUMBER($frame, useGrouping: "false") } of { $total }
preview-source = source frame { NUMBER($frame, useGrouping: "false") }
