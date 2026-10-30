# Negative Craft

`crates/pipeline/src/film/mod.rs` converts a color negative or a black-and-white negative.
It runs on scene-linear transmission, before white balance, and hands a scene-linear positive to
the rest of the pipeline. Auto tone, Auto black-and-white mix, white balance, and the selector sample
that positive, not the negative. On a raw the density is white-balanced camera RGB from before the
colour matrix, the look table, and the clamp that makes an ordinary photo: the orange mask is still
in those pixels. The camera's fitted look then carries the positive to Rec.2020, with each row
scaled so white stays white, and the clamp happens once, after that. The look's own white shift
stays on the ordinary photo path, where the hue table cancels it. A JPEG has no camera RGB. It stays on Flat, and the curve is only as true as the
rebate that survived the tone curve. The GPU kernel `film_k`
(`crates/gpu/src/wgsl/map.wgsl`) is the same formula, including the 45 words from `FilmKernel::words`.
Settings and commands are `film.*`. A slide (`colorPos`, `bwPos`) is still
accepted by `film.setStock` so an old frame opens; it is not on the panel, because a slide is
already a positive.

## Using it

- **App:** **Window ▸ Edit Sections ▸ Negative Craft** shows the section above **Light**. It stays
  hidden until then. Choosing the menu item again hides it, as does a right-click on the header
  and **Hide Negative Craft**. A photo that is already converted still shows the section.
  Pick **Color Neg** or **B&W**, then **Convert**.
  The mask swatch is the blank film. **Auto** measures again. **Border** picks the blank film
  when the border is wrong. On a color negative the click has to be orange: a pale fringe, a
  neutral holder, or a white-balanced rebate is refused, and so is a blown-out lamp. **Gray** (color only)
  picks one tone that should print gray; **Shadow** and **Highlight** under **More** add a dark and
  a light one. Together they set the gray axis. While one of those clicks is
  waiting, a line under the buttons says where to click. **Crop** sits under that row. **Auto**
  cuts the scan to the picture, past sprocket holes and the printing on the rebate. **Adjust**
  opens the crop tool. **Scan** (Camera, Minilab, Flat) and **Print** (Neutral, Portrait, Vivid,
  Lab) are the dye correction and the paper, on a color negative. Convert uses Camera on camera
  RGB unless Minilab or Flat was already chosen; a rendered file stays Flat. **Look** (Neutral, Warm, Cool,
  Paper, Cinematic) changes color and the paper shoulder. It does not replace the border, the
  gray axis, exposure, contrast, or whether conversion is on. **Exposure**, **Contrast**, and
  **Blacks** place the print. **Margin** is a percent trimmed from each edge of the frame before
  that measurement. The blank film outside the frame stays the mask. **Auto** under Crop cuts to the
  picture and does not reprint. A crop drawn before **Convert** is the whole measurement: only that
  rectangle is read, and the surround is ignored. Orange blank film still inside it is the base.
  With none there, Convert stays off rather than taking a neutral from the picture.
  On a color negative, **Temp**, **Tint**, and the shadow, midtone,
  and highlight balances trim color. **Saturation** (color only) sits under **Blacks**. A
  black-and-white negative stops at the print sliders: the color looks stay off, so they cannot
  tint the gray. **More** on a color negative keeps the cast Auto removed, the shoulder,
  **Channel Contrast** (red, green, blue), the **Mask** numbers, scan gamma
  (Linear, 1.8, 2.2), the shadow and highlight picks, and **Match roll**. On black-and-white,
  **More** keeps the shoulder, the scan gamma, and **Match roll**.
- **Commands** (CLI, MCP, control channel). A stock, scan, scan-gamma, margin, mask, or
  pick change measures again when conversion is already on, and keeps the user's Exposure and
  Contrast. Print does not measure. Only **Convert** (`film.analyze`) sets Exposure and Contrast,
  clears the picks, and puts Channel Contrast back to 1.
  - `film.setEnabled {enabled: bool}` — on measures and prints; off leaves the pixels alone
  - `film.analyze {}` — the same measurement as **Auto** and **Convert**
  - `film.setStock {stock: colorNeg|bwNeg|colorPos|bwPos}`
  - `film.setScan {scan: camera|minilab|flat}`
  - `film.setPrint {print: neutral|portrait|vivid|lab}`
  - `film.setCapture {capture: linear|gamma18|gamma22}`
  - `film.pickBase {x, y}` — **Border**
  - `film.pickGray {x, y}`, `film.pickShadow {x, y}`, `film.pickHighlight {x, y}` — a tone that
    should print gray; the picks together set the gray axis
  - `film.setFrame {corners: [[x, y], [x, y], [x, y], [x, y]]}`
  - `film.cropToImage {}` — **Auto** under Crop; cuts to the picture and does not reprint
  - `develop.set {control: film.inset, value: 0..40}` — **Margin**. Trims that percent from each edge of the frame and measures again. The mask, Exposure, and Contrast stay. Zero measures the whole frame.
  - `film.analyzeRoll {}` — share mask color and the gray axis; each frame is exposed again so the shared gray does not move its midtone
  - `film.inspect {}` — density histogram and rebate, without writing settings
  - `preset.apply {id: lc.nc-neutral|lc.nc-warm|lc.nc-cool|lc.nc-paper|lc.nc-cine}` — the five looks,
    merged onto the keys they name

## What it does

Clear film is the per-channel base, from the rebate (or **Border**). On a color negative that
population has to be orange, on the order of unexposed color film: red well above blue. A pale
fringe, a neutral ring, or a whole-scan histogram is not Dmin. Any of those leaves the orange
mask in the print, and the paper curve turns that into a blue cast. Convert stays off and asks
for a **Border** click on the orange film. A click that fails the same test is refused. A later
Convert does not replace a stored orange click with a paler auto base. A clipped white lamp is
never the base. Among the orange rebate, Auto uses the typical color of that area, not the
brightest washed fringe. A hot pixel does not set it. Density
is `D = -log10(transmission / base)`, which removes the constant orange mask and nothing else.
Scan gamma is removed first. Linear leaves the file alone. Gamma 1.8 and 2.2 raise each channel
to that power, and the print does the same, so a measured rebate is black. Frame detection and
the mask both classify that stored scan. A rebate that only looks pale before the gamma is removed
is judged after it; if it is still not orange, it is not the base.

Density goes through Hanson's dye inverse (below). Fog stays 0: the rebate is zero density, as a
lab that has zeroed the head on unexposed film. The axis is the slope of the least coloured
whites, so those print gray. A colour that is further off that slope — a sky, foliage — stays
there. Without a rebate, Convert stays off and asks for a **Border** click: a histogram of the
picture is never Dmin (that printed dark and blue). Each channel is printed on the same paper curve:

`s_c = (D_c − fog_c) / axis_c`

`L(x) = 10^(contrast · (x − mid) / 0.6)`, `scene = 0.18 · (L(s) − L(black)) / (1 − L(black))`

0.6 is a typical negative's gamma. At Contrast 1 the print gives back the scene the film recorded;
higher is a harder paper, lower a softer one. The curve turns about the middle: the picture's
middle density prints middle gray, 0.18, at any contrast. The black is the density that prints 0,
just under the picture's 0.5th percentile and never under clear film; **Blacks** moves it. Below
0.002 a short toe keeps the darkest grain from going flat. Convert sets the middle to the
picture's log-average (the mean of its 5th to 95th percentile), and the contrast that prints the
99th percentile at 1.0, the top of the paper. When the contrast range (0.25–4) cannot get there,
Exposure makes up the rest. **Exposure** is a gain in stops after the curve.

**Channel Contrast** trims one channel's slope about the middle, `s' = mid + trim · (s − mid)`,
after the measurement, so middle gray stays put and a re-measure does not undo it. The paper aim
(Neutral, Portrait, Vivid, Lab) multiplies the print at unit luminance. **Saturation** pushes color
away from luminance and stops at the gamut edge, so it never darkens or clips a color; neutrals do
not move. **Temp**, **Tint**, and the shadow, midtone, and highlight balances are gains at unit
luminance: they move hue, not brightness. The shadow balance fades out between 3 and 0.5 stops
under middle gray, the highlight balance fades in between 0.5 and 2.5 stops over it, and the
midtone balance covers the whole picture. The **Shoulder** then rolls off above paper white.

The picks (**Gray**, **Shadow**, **Highlight**) are the filter pack: one pick sets the offsets
through it; picks that span at least 0.05 density also fit the slopes. **Match roll** copies the
mask colour and that pack, then places each frame's exposure again.

The mask was built for color paper, not for a camera. Magenta dye absorbs blue, and cyan dye
absorbs green and a little blue (Hanson, J. Opt. Soc. Am. 40, 1950). The residual a scanner still
sees is removed with Hanson's inverse: `C = Dr`, `M = Dg − cg·C`, `Y = Db − cb·C − mb·M`. Camera
uses a wider residual than Minilab. Convert selects it when the pixels really are camera RGB,
unless the scan was already set to Minilab or Flat. The rows are not scaled to sum to 1. Flat
keeps integral density and is what a JPEG or other rendered file uses: Camera Hanson there, with
fog 0 and axis 1, tints a gray the file has already made equal. A gray subject lays down equal dye amounts; the camera's integral
densities of that gray are not equal, and Hanson recovers the dye amounts. Dye amounts below zero are clamped. Black-and-white uses luminance density, one
channel, and no aim. The panel does not offer a slide. A frame already set to `colorPos` or `bwPos`
stays a positive and only applies contrast.

After the conversion, Light uses the HDR tone map, so Light's Contrast, Whites, and Blacks work on
the print as they do on a raw.

The frame is the picture inside the rebate. A black holder around the film is not the border: the search crosses it and stops where the flat blank film ends in one step, even when the picture is nearly as bright as that film. Deep shadows running off the edge of the frame fade in instead, so they are kept. Each edge is read across the picture the other direction found, so a bright surround down one side does not look like sprocket holes along every row, and a pixel brighter than the base in any channel is not film. Orange for a color negative, bright for black-and-white, dark for a slide. The rebate is that flat band, so a thinner washed rim where the film meets the lamp is not the base, and neither is a bright patch inside the picture. When the band has no inner edge, Convert uses the sprocket and clear-film gate instead, and stores whichever quad it found. **Auto** under **Crop** uses it, and when that quad is the whole scan it finds the picture inside the sprockets instead. When no rebate is found the previous quad is kept and **Convert** and **Auto** say so. A roll shares the mask color and the gray axis. Density is already above each frame's own rebate, so the axis is copied rather than scaled. Each frame's exposure is solved again with that axis, so the shared gray does not shift the midtone. A frame whose rebate is more than half a stop from the roll is reported and still converted.

## Scanning

The rebate has to be in the file. A few millimetres of blank film at the edge is the measurement.
Lock the camera's white balance for the roll (Daylight, or one custom balance) so the rebate stays
the same orange on every frame. Expose so that border is bright and not clipped: a pure-white
rebate has no mask color left, and **Border** refuses it. Convert and **Border** always read the
original scan, never the print, so a second Convert or a click after a bad Auto still sees the
orange. **Border** is a probe on that click (a small median, so grain does not set Dmin). A
clipped lamp and the black holder are refused; the orange film is not. A pale or neutral spot
is refused the same way. A raw holds that measurement more faithfully than a JPEG of the same
negative: a tone-curved file is only as true as the rebate that survived, and a base that is not
orange is not printed.

## Limits

- The fitted slope is kept within 0.4–2.5. A contact sheet of many pictures can need more; pick
  two neutrals there.
- A JPEG or a flatbed file has already been through a tone curve and a working-space conversion.
  The density formula is exact for linear camera RGB. On a rendered file it is the same curve
  applied to whatever rebate survived, and a base that is not orange is refused instead of printed.
- A frame with no rebate keeps the previous border. **Match roll** shares the mask color ratio and
  each frame's own rebate level, so it does not supply a mask to a frame that has none.
- A clipped rebate is refused as a base. The mask color is gone from that file. Noise on a rebate
  that is actually in the scan is averaged out; noise cannot invent a mask the file does not hold.
- No manufacturer's stock profile and no scanner table. The dye numbers are a typical residual of
  a wide filter against a narrow one. The formula is the transmission model, Hanson's inverse, the
  film characteristic curve, and the gray axis. No plugin binary is used here.
