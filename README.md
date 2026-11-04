# h264mosh

Datamoshing for H.264, done on the bitstream.

h264mosh has its own H.264 parser and writer. It edits frames and macroblocks directly in the coded stream and never re-encodes while moshing. The output is meant to always be a conforming H.264 stream: headers are renumbered and parameter sets are inserted where they are needed, so nothing is left for a decoder to guess. It should play everywhere, and decoders should agree on the picture.

> **Status:** early development. This README describes what the project is meant to become; it is not a usage guide yet, and nothing here is stable.

## The idea

A video is a chain of pictures: keyframes (I) that stand alone and predicted frames (P) that describe change relative to earlier pictures. Datamosh is what happens when that chain is tampered with: remove a keyframe and the next frames keep painting their motion onto the wrong picture; repeat one P-frame and its motion keeps going.

Doing this on the stream, instead of on decoded pixels, means the effects are real codec behaviour rather than imitations of it, and editing is instant: the picture is never computed, only bytes are rewritten.

## Planned

### Frame level

- Drop keyframes (all of them, or chosen ones) so motion bleeds across cuts.
- Repeat any frame any number of times ("bloom" for P-frames, freeze for keyframes).
- Reorder, delete and move frames freely, with the stream renumbered so the result is still valid. This works on streams with B-frames too, straight from the camera or encoder: a B-frame moved elsewhere predicts from its new neighbours.
- Join several clips into one stream, then mosh across the joins.

### Macroblock level

- Edit motion vectors: scale, shift, random jitter, or scale the coded vector differences so the error spreads through prediction. B-frames too: the motion they predict from the frames around them (direct prediction) is worked out the way a decoder does, so it can be changed like the coded motion.
- Stop intra macroblocks from healing the picture, or remove all residual so the picture is built from prediction alone.
- Rewrite intra prediction directions, so keyframes smear along wrong directions; set or raise quantizers, and scale transform coefficients by frequency.
- Make blocks predict from older reference frames (echoes), and copy macroblocks from one frame into another.
- Drift brightness and colour through weighted prediction, and change the deblocking strength; these only touch slice headers, so they work on any stream.
- Every change stays within what the decoder is allowed to compute, so the result is still a valid stream.
- All of this works on CAVLC and CABAC streams alike, straight from the camera or encoder: macroblocks are parsed into one model and written back with the same entropy coding (not interlaced MBAFF streams).
- Apply effects to chosen frames, in layers, in a chosen order, optionally limited to an area of the picture.
- Later: a "motion brush" to paint those areas, partial and proportional versions of the healing effects, and moving the motion of one clip onto the picture of another.

### Editor

A desktop app (Rust, egui) built around the idea that nothing is final and everything can be seen:

- A frame timeline you can zoom from thumbnails out to the whole clip in view, a barcode of I and P frames, with deleted frames kept as empty slots until you render; a time bar under the preview to scrub through the result. Hovering tells the time and the frame.
- Deleting can keep the length: the frame after a deleted one is shown in its place, so its motion is applied twice and everything after it stays at its time.
- Direct manipulation: select, drag, duplicate, delete, with unlimited undo and project files.
- Effect lanes under the frames, with draggable edges, parameters and bypass.
- Showing the invisible: motion vector overlay, a motion curve, and a meter of how much of each frame is intra coded (how fast it heals).
- Comparison: wipe, side by side, flicker and difference against the original, plus variants (A/B/C) of the same edit and a grid that tries four parameter values at once.
- Ramps (parameters that change over time) and presets.
- Previews decoded in the background at reduced size, by the same software decoder that is used for the export, so what you see is what you get.
- Clips are re-encoded to CAVLC when they are opened, the most tested path, in the way they should mosh: motion from flowing (whole blocks move together, steady from frame to frame) to detailed, healing from slow to fast (how soon the real picture breaks through), B-frames and reference frames. An experimental option keeps their original coding (CABAC, MBAFF) and uses their streams exactly as they are.
- The clips' sound plays with the preview, at its own time.
- Export as MP4 or MKV, with the mosh baked in or as the moshed stream itself, keeping the clips' audio and subtitles; or as a raw H.264 stream.
- The interface is English, but written to be translated from the start.

### Command line

The same engine for scripting: prepare clips, mosh them with a plan, inspect streams, and check that parsing and rewriting reproduce a stream bit for bit.

### Later, bigger

- Sound that is edited along with the frames (now it keeps its own time).

## Principles

- **Bitstream first.** No re-encoding while moshing; the output is a valid stream.
- **Correctness is tested.** Parsing and rewriting must reproduce streams bit for bit across a wide matrix of encoder settings, and edited streams must decode cleanly in an independent decoder.
- **ffmpeg is the only external tool**, run as a separate process rather than linked.
- **Cross-platform** (Windows, macOS, Linux).
- **Simple code.** Optimizations are measured, and those that do not pay off stay out.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
