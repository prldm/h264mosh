# English UI text; the fallback for every other language.
# Keys are referenced from the code as tr!("key"); see src/i18n.rs.
# Frame numbers are 0-based, in decode order, like on the command line.

app-title = h264mosh
window-title = { $project } – h264mosh
window-title-unsaved = • { $project } – h264mosh
empty-hint = Open one or more clips or a project to start, or drop them here.
open-button = Open…
open-tooltip = Open a project, or video clips: they are re-encoded for moshing, the way you choose; a raw .264 stream that can be edited opens as it is.

## Menus

menu-file = File
menu-open = Open…
menu-recent = Open recent
menu-save = Save project
menu-export = Export video…
menu-new-clips = New clips
import-as-coded = Keep the original coding: CABAC, MBAFF (experimental)
import-as-coded-tooltip = Clips opened from now on are not re-encoded: their H.264 stream is used exactly as the camera or encoder made it, and nothing is asked about encoding. Clips joined this way have to be coded alike. Macroblock effects on CABAC streams are new; interlaced MBAFF streams take frame edits and the picture effects only. Without this, clips are encoded to CAVLC for moshing, as chosen when they are opened (raw CAVLC streams are used as they are).
menu-edit = Edit
menu-undo = Undo
menu-redo = Redo
menu-drop-keyframes = Drop keyframes
menu-delete-keeps-length = Deleting keeps the length
menu-delete-keeps-length-tooltip = A deleted frame is replaced by the frame after it, shown once more (at the end, by the frame before it), so everything after it stays at its time and in step with the sound. Its motion is applied twice, which smears further. Switched off, deleted frames are cut out and the video gets shorter.
menu-help = Help
menu-shortcuts = Keyboard shortcuts
filter-all = Projects and videos
filter-project = h264mosh project
filter-video = Video
filter-mp4 = MP4 video
filter-mkv = Matroska video
filter-raw = Raw H.264 stream

## Encoding new clips (asked when they are opened)

import-title = Encode for moshing
import-intro = The clips are encoded again before you edit them. How they are encoded decides how they mosh.
import-raw-note = A raw CAVLC stream that can be edited opens as it is; these choices apply only if it has to be encoded.
import-motion = Motion
motion-flowing = Flowing
motion-balanced = Balanced
motion-detailed = Detailed
motion-flowing-about = Whole blocks move together and follow the main motion: the picture slides and melts in calm, even pieces, steady from frame to frame.
motion-balanced-about = Blocks can split into four to follow smaller movements: the smear breaks up a little more.
motion-detailed-about = Small blocks chase every movement, noise included: busier and more ragged, with stray blocks going their own way.
import-healing = Healing
healing-slow = Slow
healing-normal = Normal
healing-fast = Fast
healing-slow-about = After a dropped keyframe the frames carry little besides motion: the smear lasts longest. The whole picture is a little softer.
healing-normal-about = The new picture seeps through the smear gradually, as detail and colour.
healing-fast-about = The new picture comes back quickly, much of it as sharp squares breaking through the smear here and there.
import-bframes = B-frames
bframes-none = None
bframes-pyramid = as references
bframes-pyramid-tooltip = Some B-frames are references for the others, as most encoders do it: moving one changes its neighbours too, and the motion is a little less steady.
bframes-none-about = Every frame is a P-frame and pushes the smear on by one step: the classic datamosh, and every effect works on every frame.
bframes-about = B-frames sit between the P-frames and blend the pictures before and after them: the smear moves in smaller, smoother steps, while the P-frames jump further and heal a little more. A B-frame moved elsewhere takes its motion from its new neighbours. The brightness and colour drifts skip B-frames.
import-refs = Reference frames
refs-one-about = Every frame predicts from the one before it: the classic datamosh.
refs-more-about = Blocks may also predict from older frames, which the Echo effect plays with. Frame edits work as well, but motion may come from older frames.
import-encode = Encode
import-recommended = Recommended
import-recommended-tooltip = Flowing motion, normal healing, no B-frames, one reference frame: steady motion, and few blocks breaking through.

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
export-button = Export video…
export-tooltip = Save the last render as an MP4 or MKV video, or as a raw H.264 stream.
export-pending-tooltip = Render the pending changes first.

## Background jobs and messages

cancel-button = Cancel
stage-starting = Starting…
stage-encoding = Encoding clip { $clip } of { $clips }
stage-copying = Copying the stream of clip { $clip } of { $clips }
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
delete-tooltip = Delete the selected frames (Del). They stay as empty slots until the next render; the frame after them takes their place if Edit ▸ Deleting keeps the length is on.
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
zoom-tooltip = All the way left, the whole timeline is in view. Ctrl + mouse wheel over the timeline zooms too.
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
card-tooltip-output = Output frame { NUMBER($frame, useGrouping: "false") } at { $time }
card-tooltip-output-range = Output frames { NUMBER($from, useGrouping: "false") }–{ NUMBER($to, useGrouping: "false") } from { $time }
card-tooltip-copies = Shown { $count } times
card-tooltip-deleted = Deleted: removed at the next render
card-tooltip-deleted-fill = Deleted: at the next render, the frame after it takes its place
card-tooltip-filling =
    { $count ->
        [one] 1 of them in place of a deleted frame
       *[other] { $count } of them in place of deleted frames
    }
card-tooltip-pending = Changed, not rendered yet
card-tooltip-removed = Deleted here: { $frames }

## Effects

effects-title = Effects
effects-none = No effects yet. Select frames and add one.
effects-need-macroblocks = Most effects cannot edit the macroblocks of this stream (interlaced MBAFF, slice groups or SP/SI slices): open the clips re-encoded, with "Keep the original coding" switched off under File, New clips. The picture effects work on any stream.
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
effect-mv-scale-about = Multiplies the motion vectors: movement gets stronger, reversed or frozen. P- and B-frames.
effect-mv-shift = Motion shift
effect-mv-shift-about = Adds an offset to every motion vector, so everything slides. P- and B-frames.
effect-mv-jitter = Motion jitter
effect-mv-jitter-about = A random offset for each macroblock. P- and B-frames.
effect-mvd-scale = Vector error
effect-mvd-scale-about = Multiplies the coded vector differences; the error spreads through the prediction, more chaotic than motion scale.
effect-kill-intra = Freeze
effect-kill-intra-about = Intra macroblocks of P- and B-frames are skipped, so moshed areas never heal.
effect-zero-residual = Ghost
effect-zero-residual-about = Drops all residual data: pictures come from prediction only, no new detail comes in.
effect-ref-shift = Echo
effect-ref-shift-about = Blocks of P- and B-frames predict from other reference frames (P-frames from older ones), so moving parts leave echoes. Needs clips opened with several reference frames.
effect-ref-shift-one-ref = This stream has one reference frame, so nothing changes. Open the clips again after choosing more reference frames under File ▸ New clips.
effect-motion-transplant = Borrowed motion
effect-motion-transplant-about = P- and B-frames move the way another frame moved: its motion vectors, at the same places, replace or mix with their own.
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
sound-tooltip = Sound on or off. The clips' sound plays at normal speed, at its own time: frames cut out, repeated or moved do not move it.
sound-failed = No sound: { $reason }
timebar-tooltip = Frame { NUMBER($frame, useGrouping: "false") } at { $time } · source frame { NUMBER($source, useGrouping: "false") }, { $kind }

## Export

export-title = Export video
export-format = Format
export-mp4 = MP4
export-mkv = MKV
export-raw = H.264 stream
export-mp4-about = Plays almost everywhere.
export-mkv-about = Takes any audio and subtitles as they are.
export-raw-about = The moshed stream itself (.264), bit for bit, and nothing else.
export-video = Video
export-baked = Mosh baked in
export-baked-about = Decoded and encoded again: plays, seeks and edits like any video. This is what the preview shows.
export-quality = Quality
quality-high = High
quality-lossless = Lossless (large, for further editing)
quality-small = Smaller file
export-moshed = Moshed stream as it is
export-moshed-about = Not encoded again. Players decode the mosh themselves; seeking and editors may struggle.
export-keep-tracks = Keep the other tracks (audio, subtitles)
export-keep-tracks-about = The clips' audio and subtitles go along unchanged and end with the video. They keep their own time: frames cut out, repeated or moved do not move them (deleting that keeps the length keeps them in step). From several clips, the first audio track of each is joined.
export-no-tracks = Raw H.264 streams have no other tracks.
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
shortcut-save-as = Save the project under another name
