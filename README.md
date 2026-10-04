# h264mosh

H.264 datamosh bitstream szinten, saját Rust parserrel. A frame-eket nem dekódolja a végső képig, hanem közvetlenül a streamet írja át: frame-szinten a NAL unitokat és a slice headereket, makroblokk-szinten (CAVLC) a mozgásvektorokat, a makroblokk-típusokat és a reziduálokat.

## Használat

```bash
# 1. klipek átkódolása mosh-barát beállításokkal (ffmpeg/libx264: bframes=0, ref=1, weightp=0)
#    és összefűzése egy nyers streambe; kiírja a klipek frame-tartományait és a keyframe-eket.
#    --cavlc: CAVLC entrópiakódolás, a makroblokk-effektekhez kell
h264mosh prep vidi1.mp4 vidi2.mp4 -o out/clips.264 --cavlc

# 2. moshing: frame-műveletek
h264mosh mosh out/clips.264 -o out/cut.264   --drop-keyframes 696          # csak a klipváltásnál
h264mosh mosh out/clips.264 -o out/all.264   --drop-keyframes              # minden keyframe az első után
h264mosh mosh out/clips.264 -o out/bloom.264 --drop-keyframes 696 --dup 700:48

# 2b. makroblokk-effektek (frame-műveletekkel kombinálhatók)
h264mosh mosh out/clips.264 -o out/drift.264 --drop-keyframes 696 --kill-intra --mv-add 4:0 --fx-frames 696-790
h264mosh mosh out/clips.264 -o out/ghost.264 --drop-keyframes 696 --zero-residual --fx-frames 696-780

# 3. dekódolás és mp4 kódolás (ffmpeg)
h264mosh render out/bloom.264 -o out/bloom.mp4
```

A frame-sorszámok mindig a **bemenet** frame-jeire vonatkoznak, 0-tól, dekódolási sorrendben. B-frame nélkül ez megegyezik a lejátszási sorrenddel. Tartomány-szintaxis: `100`, `100-200`, `300-` (a végéig), vesszővel elválasztva több is megadható.

### Frame-műveletek

| Opció | Hatás |
|---|---|
| `--drop-keyframes [FRAMES]` | I-frame-ek (IDR és nem-IDR) kidobása. Érték nélkül: az első kivételével mind. |
| `--drop FRAMES` | Bármilyen frame kidobása (P is). |
| `--dup FRAMES:COUNT` | A frame után COUNT darab másolat beszúrása. Többször is megadható. IDR-re is működik (kimerevítés). |
| `--no-deblock` | A deblocking filter kikapcsolása minden slice-ban. Ha a PPS nem engedi, a PPS-t is átírja. |

### Makroblokk-effektek (CAVLC stream kell)

| Opció | Hatás |
|---|---|
| `--mv-scale X[:Y]` | A mozgásvektorok szorzása (pl. `2`, `1.5:-1`). |
| `--mv-add DX:DY` | Eltolás pixelben (negyed pixel pontossággal) minden mozgásvektorhoz: minden „csúszik”. |
| `--mv-jitter PX` | Véletlen eltolás makroblokkonként, legfeljebb PX pixel (`--seed`). |
| `--mvd-scale X[:Y]` | A kódolt vektor-*különbségek* szorzása. A hiba a predikción keresztül szétterjed: kaotikusabb, mint az `--mv-scale`. |
| `--kill-intra` | A P-frame-ek intra makroblokkjai helyére skip kerül, így ott nem frissül a kép. |
| `--zero-residual` | Minden reziduál törlése: a kép csak predikcióból épül, nem jönnek be új részletek („ghost”). |
| `--fx-frames FRAMES` | Mely frame-ekre hatnak az effektek (alapból mindre). |

A mozgásvektor-effektek P-frame-eken működnek, I- és B-frame-eken kimaradnak. A vektorok valódi vektorként szerkesztődnek: a tool kiszámolja a predikciót, módosítja a vektort, majd újrakódolja a különbséget, így a többi makroblokk mozgása nem változik. Ha egy skip makroblokk új vektora eltér a predikálttól, explicit P_L0_16x16 lesz belőle.

### Diagnosztika

```bash
h264mosh info vidi1.mp4 --frames      # stream szerkezete; mp4/mkv bemenetnél ffmpeg-gel kinyeri a H.264-et
h264mosh info out/clips.264 --mbs     # frame-enként intra/inter/skip makroblokkok és átlagos mozgás
h264mosh roundtrip vidi1.mp4 --mb     # parse + újraírás bitre azonos-e (--mb: makroblokk-réteg is)
```

Egyetlen külső függőség: **ffmpeg** (libx264-gyel) és a mellé tartozó ffprobe. Keresési sorrend: `--ffmpeg PATH` opció, `$H264MOSH_FFMPEG`, végül a PATH; az ffprobe-ot az ffmpeg mellől veszi. Nyers `.264` bemenethez (`mosh`, `info`, `roundtrip`) nem kell ffmpeg. A `prep` extra libx264-opciókat a `--x264-params "k=v:k=v"` kapcsolóval kap.

## Library API (GUI-hoz)

A szerkesztés adatként írható le: egy `mosh::Timeline` a kimeneti frame-ek listája. Minden bejegyzés egy bemeneti frame-re mutat (`src`), és saját `effects: Vec<MbEffect>` listája és `seed`-je van. A véletlenszerű effektek (jitter) a bejegyzés seedjét használják, így ha máshol szerkeszted a timeline-t, az adott frame nem változik meg. A frame-ek szabadon sorba rendezhetők, ismételhetők és kihagyhatók; a `mosh::render(&Source, &Timeline, &RenderOptions)` ebből szabályos streamet készít. A CLI `MoshPlan`-je is erre fordul le.

```rust
let src = Source::new(Stream::parse(&bytes)?);
let mut tl = Timeline::identity(src.len());
tl.entries.swap(40, 41);
tl.entries[50].effects.push(MbEffect::MvScale(2.0, 2.0));
let (stream, report) = render(&src, &tl, &RenderOptions::default())?;

// előnézet: elég a szerkesztés előtti utolsó IDR-től újradekódolni
let k = 50;
let start = report.restart_point(k);
let bytes = segment(&stream, &report.frames, start..k + 1)?;
```

A GUI-hoz fontos tulajdonságok:
- **Szálbiztos:** a `Source`, a `Timeline` és a `Stream` `Send + Sync`, így háttérszálon renderelhetők (teszt figyeli).
- **Frame-térkép:** a `RenderReport::frames` megmondja, melyik kimeneti frame melyik bemeneti frame, IDR-e, és mely NAL-okból áll. A `Stream::write_with_offsets` megadja a bájt-pozíciókat.
- **Szakasz-dekódolás:** a `segment()` önállóan dekódolható szakaszt ad, a szükséges SPS/PPS-t elé teszi.
- **Hibatűrés:** ha egy frame-en az effekt hibát dob vagy panikol, az a frame változatlan marad, és a `RenderReport::failed_effects` jelzi. A render ettől nem áll le.

## Teljesítmény

A frame-ek a bitstream szintjén függetlenek (képpontot nem számolunk), ezért a nehéz munka (slice-ok parse-a, makroblokk-effektek, újraszerializálás, ellenőrzések) frame-enként párhuzamosan fut, `rayon`-nal. A kimenet bitre azonos az egyszálas futáséval. A `--threads N` kapcsolóval korlátozható (alapból minden logikai mag).

Mérés Ryzen 9 5950X-en (16 mag), CAVLC streamek, a `scripts/bench.sh` szerint:

| | eredeti (1 szál) | most |
|---|---|---|
| effektek, 720p, 1208 frame | 10,2 mp | 0,80 mp |
| effektek, 1080p, 512 frame | 11,2 mp | 0,93 mp |
| `info --mbs`, 1080p | 5,5 mp | 0,49 mp |
| `roundtrip --mb`, 720p | 9,4 mp | 0,56 mp |

A sok szálon a memória-sávszélesség a korlát, ezért a memóriaforgalom csökkentése is számít:
- a makroblokk-réteg csak a nem nulla együttható-blokkokat tárolja
- a `Macroblock` kompakt (216 bájt, teszt őrzi)
- a szomszéd-adatok csak két makroblokk-sornyi helyet foglalnak, nem egész képnyit
- a CAVLC-dekódolás keresőtáblás
- a bináris `mimalloc` allokátort használ.

A library-t használó programoknak (pl. a GUI-nak) is érdemes `mimalloc`-ot beállítaniuk globális allokátornak.

Kiadandó binárishoz: `cargo build --profile dist` (LTO, egy szálon pár százalékkal gyorsabb, de a fordítás lassú, ezért a sima `--release` nem használja). A `scripts/ref_outputs.sh` két build kimenetét veti össze bitre, optimalizálások ellenőrzéséhez.

## Hogyan működik

- **Parser** (`nal.rs`, `sps.rs`, `pps.rs`, `slice.rs`): Annex B felbontás, emulation prevention, teljes SPS (VUI, HRD, scaling listák), teljes PPS (FMO, 8x8 kiterjesztés), teljes slice header (ref list modification, pred weight table, MMCO). Minden olvasott elem visszaírható.
- **Access unitok** (`stream.rs`): a NAL-okat a szabvány 7.4.1.2.4 pontja szerint csoportosítja frame-ekbe.
- **Makroblokk-réteg** (`mb.rs`, `cavlc.rs`, `cavlc_tables.rs`): a CAVLC slice data teljes parse-a és írása. Benne van minden I/P/B makroblokk-típus, sub-partíció, ref_idx, mvd, CBP, I_PCM és minden reziduál-együttható, 4:0:0/4:2:0/4:2:2/4:4:4-ben, 8–14 biten. Az nC-kontextust és a QP-predikciót írásnál újraszámolja, így a szerkesztett slice konzisztens marad. A VLC-táblákat (a szabvány 9-4, 9-5, 9-7…9-10 táblái) a `scripts/gen_cavlc_tables.py` generálja.
- **Mozgásvektor-predikció** (`motion.rs`): a szabvány 8.4.1 pontja szerint P slice-okra (P_Skip, medián, 16x8/8x16 irányított, C→D helyettesítés).
- **Effektek** (`effects.rs`) és **timeline/render** (`mosh.rs`): a szerkesztés után a paraméterkészleteket a szükséges helyre teszi, újraszámozza a `frame_num`/POC/`idr_pic_id` értékeket, így a kimenet szabályos stream.
- **Külső eszköz** (`pipeline.rs`): az ffmpeg-et folyamatként hívja (nem linkeli), így minden platformon ugyanúgy működik, és licenc szempontból is független marad.

## Tesztek

```bash
cargo test --release                                    # unit + integrációs tesztek (ffmpeg/libx264 kell)
bash scripts/roundtrip_matrix.sh vidi2.mp4              # ~56 enkóder-beállítás (ffmpeg), bitre azonos roundtrip, makroblokk-szinten is
python scripts/compare_trace.py target/matrix/*.264     # slice headerek összevetése az ffmpeg trace_headers kimenetével
```

Az integrációs tesztek fő ellenőrzései:
- **Header-átírás:** a header hosszát megváltoztató átírás után (CABAC és CAVLC) a dekódolt képkockák MD5-je változatlan.
- **MV-predikció:** átstrukturálás után a kép pixelre azonos marad. Ilyen átstrukturálás, ha minden skip makroblokk explicit lesz, vagy minden partíció 8x8-ra bomlik ugyanazzal a mozgással. Ez a predikciót a saját dekóderünktől függetlenül, az ffmpeg-gel ellenőrzi.
- **Effektek és timeline:** az effektek és az átrendezett timeline-ok hiba nélkül dekódolnak.
- **Szakaszok:** az IDR-től önállóan dekódolt szakasz frame-ről frame-re egyezik a teljes dekódolással.
- **Hibatűrés:** egy szándékosan elrontott frame csak saját magát ejti ki.

A tesztek is a `$H264MOSH_FFMPEG`-et vagy a PATH-on lévő ffmpeg-et használják; ha nincs ffmpeg, az integrációs tesztek kihagyódnak.

## Korlátok

- Frame-szerkesztéshez (dobás, ismétlés, sorrendcsere) B-frame nélküli, progresszív stream kell; a `prep` ilyet készít.
- Makroblokk-effektekhez CAVLC kell (`prep --cavlc`). CABAC, MBAFF, FMO és SP/SI slice nem támogatott. Mozgásvektort csak P slice-ban szerkeszt.
- Az MMCO-t használó streameknél figyelmeztet, mert szerkesztés után a referenciák máshová mutathatnak.
- Hang nincs, a moshing megváltoztatja a hosszt.

## Licenc

MIT vagy Apache-2.0, választás szerint ([LICENSE-MIT](LICENSE-MIT), [LICENSE-APACHE](LICENSE-APACHE)). Az ffmpeg külön folyamatként fut, nincs linkelve.
