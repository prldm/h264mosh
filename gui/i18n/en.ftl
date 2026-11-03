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
menu-new-clips = New clips
import-refs =
    { $count ->
        [one] 1 reference frame
       *[other] { $count } reference frames
    }
import-refs-one-tooltip = Clips opened from now on are encoded with one reference frame: the classic datamosh.
import-refs-more-tooltip = Clips opened from now on are encoded with several reference frames, for the Echo effect. Frame edits work as well, but motion may come from older frames.
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
effects-need-cavlc = Most effects need a CAVLC stream: open the clips from video files (they are re-encoded so) instead of a raw stream. The picture effects work on any stream.
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
        [one] 1 of them is a frame of a type this effect does not change.
       *[other] { $count } of them are frames of a type this effect does not change.
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
effect-ref-shift = Echo
effect-ref-shift-about = Blocks of P-frames predict from older frames, so moving parts leave echoes. Needs clips opened with several reference frames.
effect-ref-shift-one-ref = This stream has one reference frame, so nothing changes. Open the clips again after choosing more reference frames under File ▸ New clips.
effect-motion-transplant = Borrowed motion
effect-motion-transplant-about = P-frames move the way another frame moved: its motion vectors, at the same places, replace or mix with their own.
effect-transplant = Transplant
effect-transplant-about = Copies macroblocks of another frame into the same places, all of them or some at random. Keyframes only take its intra blocks.
effect-intra-mode = Intra direction
effect-intra-mode-about = Sets the direction intra blocks are predicted from. The detail stays, so keyframes smear along the new direction, and the P-frames after them carry it on.
effect-intra-rotate = Intra turn
effect-intra-rotate-about = Turns every intra prediction direction, so edges are drawn at the wrong angle.
effect-intra-random = Intra scramble
effect-intra-random-about = Gives some of the intra blocks a random direction.
effect-intra-cascade = Intra cascade
effect-intra-cascade-about = Writes the same code into every intra direction. What it means depends on the blocks around, so the change ripples across the picture.
effect-qp-add = Quantizer shift
effect-qp-add-about = Changes the quantizer: the coded detail is scaled up or down (twice as strong every 6 steps), as far as the stream stays valid.
effect-qp-set = Quantizer
effect-qp-set-about = Sets the quantizer of every macroblock, as far as the stream stays valid.
effect-coeffs = Frequencies
effect-coeffs-about = Multiplies the transform coefficients from a frequency on: 0 removes the fine detail, large values sharpen it into ringing, negative values invert it.
effect-dc-shift = Block tone
effect-dc-shift-about = Adds to the brightness and colour of every block. In keyframes it builds up along the prediction.
effect-deblock = Deblocking
effect-deblock-about = Strength of the deblocking filter: higher smooths block edges more, lower leaves them hard.
effect-weight = Exposure drift
effect-weight-about = Brightness gain and offset of the motion prediction. It adds up from P-frame to P-frame until the next keyframe.
effect-tint = Colour drift
effect-tint-about = Colour offsets of the motion prediction: the colours drift from P-frame to P-frame.
effect-debug = Debug effect
group-motion = Motion
group-blocks = Blocks
group-intra = Intra prediction
group-residual = Detail and quantizer
group-picture = Picture
direction-vertical = vertical
direction-horizontal = horizontal
direction-dc = flat (DC)
direction-down-left = diagonal down-left
direction-down-right = diagonal down-right
direction-vertical-right = vertical-right
direction-horizontal-down = horizontal-down
direction-vertical-left = vertical-left
direction-horizontal-up = horizontal-up
summary-scale = ×{ NUMBER($x, maximumFractionDigits: 2) } / ×{ NUMBER($y, maximumFractionDigits: 2) }
summary-shift = { NUMBER($x, maximumFractionDigits: 2) } / { NUMBER($y, maximumFractionDigits: 2) } px
summary-jitter = ±{ NUMBER($px, maximumFractionDigits: 2) } px
summary-frames = { $n } frames
summary-transplant = from { NUMBER($frame, useGrouping: "false") }, { NUMBER($percent, maximumFractionDigits: 0) }%
summary-degrees = { NUMBER($degrees, maximumFractionDigits: 1) }°
summary-percent = { NUMBER($percent, maximumFractionDigits: 0) }%
summary-number = { $n }
summary-signed = { $n }
summary-coeffs = ×{ NUMBER($factor, maximumFractionDigits: 2) } from { NUMBER($percent, maximumFractionDigits: 0) }%
summary-two = { $a } / { $b }
summary-three = { $a } / { $b } / { $c }
summary-weight = ×{ NUMBER($gain, maximumFractionDigits: 2) } / { $offset }
param-horizontal = Horizontal
param-vertical = Vertical
param-horizontal-px = Horizontal (px)
param-vertical-px = Vertical (px)
param-amount-px = Amount (px)
param-pattern = Pattern {" "}
param-new-pattern = New pattern
param-frames-back = Frames back
param-from-frame = From frame {" "}
param-amount = Amount
param-mix = Mix
param-direction = Direction
param-turn = Turn
param-code = Code
param-quantizer-offset = Quantizer offset
param-quantizer = Quantizer
param-from-frequency = From frequency
param-factor = Factor
param-brightness = Brightness
param-blue = Blue
param-red = Red
param-deblock-alpha = Edge strength
param-deblock-beta = Edge threshold
param-gain = Gain
param-offset = Offset
area-limit = Only in an area
area-limit-tooltip = Limit the effect to a rectangle of the picture (whole macroblocks). The preview shows it while the effect is selected.
area-left = Left
area-top = Top
area-width = Width
area-height = Height
area-invert = Everywhere but there

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
