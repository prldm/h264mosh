# h264mosh – átadás a GUI-fejlesztéshez

Ez a dokumentum az előző munkamenet összefoglalója: hol tart a projekt, mi el van döntve, és milyen GUI-t tervezünk. A részletes használat és architektúra a [README.md](../README.md)-ben van.

## A projekt egy mondatban

H.264 datamosh **bitstream szinten**, saját Rust parserrel: frame-szintű műveletek (keyframe-dobás, P-frame duplikálás/bloom, tetszőleges átrendezés) és makroblokk-szintű effektek (mozgásvektor-szerkesztés, kill-intra, zero-residual). A kimenet mindig **szabályos** H.264 stream: a fejlécek átszámozódnak, ezért nem vész el frame, és mindenhol lejátszható.

## Állapot

Kész és tesztelt:

1. **1. fázis – frame-szint.** NAL/SPS/PPS/slice header parse és bitre pontos visszaírás; `frame_num`/POC/`idr_pic_id` átszámozás; paraméterkészletek automatikus beszúrása; `--no-deblock`.
2. **2. fázis – makroblokk-szint (csak CAVLC).** Teljes MB-réteg parse/írás (minden I/P/B típus, 4:0:0–4:4:4, 8–14 bit, PCM); P-slice MV-predikció (8.4.1); effektek.
3. **GUI-ra kész mag.**
   - `Timeline` modell, `Send + Sync`.
   - Frame-szintű hibatűrés: `catch_unwind`, `failed_effects`.
   - Szakasz-dekódolás: `restart_point`, `segment`.
   - Stabil seed bejegyzésenként.
4. **Teljesítmény.** `rayon` párhuzamosítás, `mimalloc`, ritka együttható-tárolás, kompakt `Macroblock` (216 B), kétsoros szomszéd-cache, keresőtáblás VLC, gyors bit-I/O. Eredmény: 1080p, 512 frame effektekkel ~0,9 mp (eredetileg 11,2 mp). `dist` profil LTO-val.

Ellenőrzés (ezeknek mindig zöldnek kell maradniuk):

- `cargo test --release`: 27 teszt, köztük pixel-azonossági és szakasz-dekódolási tesztek.
- `bash scripts/roundtrip_matrix.sh vidi2.mp4`: 56 enkóder-beállítás, bitre azonos roundtrip, makroblokk-szinten is.
- `python scripts/compare_trace.py target/matrix/*.264`: slice headerek összevetése az ffmpeg-gel.
- `scripts/ref_outputs.sh`: két build kimenetének bitre pontos összevetése.

Korlátok:

- Frame-szerkesztéshez B-frame nélküli, progresszív stream kell; ezt a `prep` biztosítja.
- MB-effektekhez CAVLC kell (`prep --cavlc`); MV-effektek csak P-slice-on.
- Nincs CABAC MB-szerkesztés és nincs hang.
- Teszteletlen ágak: PCM-et, FMO-t, POC type 1-et, long-term refeket egyik tesztstream sem tartalmaz.

## Fontos döntések (ne írjuk felül kérdezés nélkül)

1. **Csak ffmpeg** a külső eszköz, folyamatként hívva, nem linkelve. Az x264 CLI-t kivettük.
2. **Cross-platform** a cél (Windows-on tesztelünk, később macOS/Linux). Nincs `.exe`-feltételezés.
3. **GitHub Actions / CI egyelőre nincs.**
4. **GUI: egui / eframe** (Rust, ugyanabban a folyamatban, mint a mag). Electron nem jöhet szóba.
5. **Az előnézet és az export ugyanazt a szoftveres ffmpeg-dekódert használja.** A sérült/mosholt streameket a dekóderek eltérően jelenítik meg, ezért nincs hardveres dekódolás az előnézetben.
6. **A GUI-ban is legyen `mimalloc`** a globális allokátor.
7. **A GUI nyelve angol, de az elejétől fordíthatóra kell írni.** Minden felirat, menü, tooltip és hibaüzenet angolul jelenik meg. A felhasználóval a chatben továbbra is magyarul kommunikálunk.
   - **Nincs beégetett szöveg a UI-kódban.** Minden látható szöveg egy fordítási rétegen keresztül, kulcs alapján jön, pl. `t!("timeline.render_button")`. Az angol a alapértelmezett és a tartalék nyelv, egy fordítási fájlban.
   - **Ne legyen túltervezve:** egy bevált crate (pl. `rust-i18n` vagy a Fluent alapú `fluent`) és egy fájl nyelvenként. A választást az első lépésnél egyeztesd a felhasználóval. Ha számít a többes szám kezelése (pl. „1 change” / „3 changes”), a Fluent a jobb.
   - **Számok, idő és frame-számlálók** formázása is ezen a rétegen menjen át, ne kézzel összefűzött stringekből.
   - **A layout bírja a hosszabb szövegeket** (pl. németben gyakran 30–40%-kal hosszabbak): gombok és panelek ne fix szélességre legyenek méretezve.
   - Jobbról balra író nyelvek (arab, héber) támogatása nem cél.
8. **Stílus:** tiszta kód, nem túltervezve; minden optimalizálást mérni kell, és ami nem hoz, az kimarad.
9. **Licenc még nincs**, és a projekt még **nem git repó**. Érdemes a GUI előtt eldönteni a felhasználóval.

## A mag API-ja, amire a GUI épül

- `stream::Stream::parse(&bytes)`, `write`, `write_with_offsets`
- `mosh::Source::new(stream)`: frame-ekre bontott forrás (`aus`, `pocs`)
- `mosh::Timeline { entries: Vec<TimelineEntry> }`, `TimelineEntry { src, effects: Vec<MbEffect>, seed }`, `TimelineEntry::new(src)`, `Timeline::identity(n)`, `entry_seed(base, src, copy)`
- `mosh::render(&Source, &Timeline, &RenderOptions) -> (Stream, RenderReport)`
  - `RenderReport::frames: Vec<RenderedFrame { src, idr, nals }>`
  - `RenderReport::restart_point(k)`, `failed_effects`, `effects`
- `mosh::segment(&Stream, &frames, range)`: önállóan dekódolható szakasz (SPS/PPS elé téve)
- `effects::MbEffect`: `MvScale`, `MvAdd` (negyed pixel), `MvJitter`, `MvdScale`, `KillIntra`, `ZeroResidual`, plus a debug célú `ExplicitSkips`, `SplitTo8x8`
- `mb::parse_slice_data`, `mb::MbContext`, `motion::decode_motion`: MB-statisztikákhoz és MV-overlayhez (lásd `info --mbs` a `main.rs`-ben)
- `pipeline::Tools::find`, `pipeline::prep`, `pipeline::render`, `pipeline::probe`, `pipeline::load_annexb`

Valószínűleg szükséges mag-bővítések a GUI-hoz:

- `serde` deriválás a projektfájlhoz (`Timeline`, `MbEffect`).
- Rámpák: paraméterek frame-enként, a timeline-ba fordítva.
- Később területi maszk az effektekhez (mozgásecset).

## A megbeszélt UI-koncepció

Alapelvek:

1. **Azonnali visszajelzés:** a UI soha nem fagy le, minden háttérszálon számolódik, előbb kicsinyített előnézet jön.
2. **Semmi sem végleges:** az eredeti klip változatlan, a szerkesztés rétegekben történik, korlátlan visszavonással.
3. **Közvetlen kezelés:** kijelölés és húzás, nem űrlapok.
4. **Láthatóvá tenni a láthatatlant:**
   - I/P jelölés a frame-eken
   - mozgásgörbe
   - MV-nyíl overlay
   - gyógyulásjelző (intra makroblokkok aránya)

Fő ablak:

- **Fejléc:** projekt, variánsok (A/B/C…), export.
- **Nagy előnézet:** összehasonlító módokkal (wipe, egymás mellett, villogtatás, különbségtérkép).
- **Timeline:** frame-sáv I/P jelöléssel, mozgásgörbe, effektsávok (színes blokkok, húzható szélekkel).
- **Jobb oldalt beállítópanel:** paraméterek, rámpa, „próbálj 4 értéket” (2×2 rács), presetek.

**Frame-szintű kézi szerkesztés** (ezt a felhasználó kifejezetten kérte):

- A timeline folyamatosan zoomolható egészen a frame-enkénti kockákig. Minden kockán: bélyegkép, forrás-sorszám, típus (I/P), effekt-jelölés.
- **Törlés:** a helyén **üres, szaggatott keretes hely marad** áthúzott sorszámmal és visszaállító gombbal, egészen a renderelésig.
- **Duplikálás:** egymásra rakott lapok darabszámmal (×3), görgővel állítható.
- **Áthelyezés:** fogd és vidd, a hely jelölést kap.
- **Kijelölés:** fájlkezelő-szerű kattintás / Shift / Ctrl.
- **Renderelésre váró jelzés:** a módosított frame-ek fölött sárga csíkos sáv, a fejlécben „N változás nincs renderelve”.
- **Kézi Render gomb** (alapértelmezés): ekkor záródnak össze az üres helyek. Ahol törlés volt, vékony emlékeztető jel marad. Opcionális automatikus mód.
- Lejátszáskor az utoljára renderelt állapot látszik.
- A „törölve” állapot csak a GUI-modellben létezik (tombstone); rendereléskor egyszerűen kimarad.

Egyéb:

- Gyorsbillentyűk: `D` keyframe dobás, `B` bloom (görgővel darabszám), `J/K/L` lejátszás, `\` az eredeti mutatása.
- Presetek kis animált előnézettel.
- Projektfájl, undo/redo.
- Renderjelző sáv a timeline felett.

## GUI-architektúra javaslatok

1. **Render-cache frame-enként**, kulcs: (forrás-frame, effektek, seed). Szerkesztéskor csak az érintett frame-ek számolódnak újra.
2. **Tartalom-alapú cache a dekódolt képekhez** lánc-hash-sel. A hash az előző frame hash-éből és a frame saját bájtjaiból jön, IDR-nél újraindul. Ez pontos érvénytelenítést ad, az A/B variánsok közös része pedig egyszer dekódolódik.
3. **Előnézet:** háttérszálon az ffmpeg folyamatból jönnek a nyers frame-ek kicsinyítve (pl. 640 px), a `restart_point`-tól. Megjelenítés GPU-textúraként; a YUV→RGB konverzió shaderben is mehet.
4. **Mosh-sajátosság:** ha nincs IDR, a mély pozíciók előnézetéhez messziről kell dekódolni. Ezt a háttérben előre dekódolt cache enyhíti.

## Ötletek a megkülönböztetéshez (későbbre)

**Ezek egyike sincs még implementálva.** A motor szerkezete lehetővé teszi őket (makroblokkonként dolgozik, és a mozgás dekódolható), de mindegyikhez új kód kell:

- **mozgásecset:** területi effekt makroblokk-maszkkal. Új: egy maszk paraméter az effektekhez, amit az `effects::apply` makroblokkonként figyelembe vesz.
- **mozgás-átültetés:** egy klip mozgása egy másik klip képén. Új effekt: egy másik frame dekódolt mozgásának átvétele, azonos felbontásnál.
- **gyógyulás-szabályzó:** a kill-intra és a zero-residual részleges, arányos vagy területi változata.
- **látható mozgásvektorok:** csak GUI-munka, a `motion::decode_motion` már szolgáltatja az adatot.

Versenytársak (2026-10): [rustjay-mosh](https://github.com/BlueJayLouche/rustjay-mosh) (egui NLE, csomagszintű keyframe-dobás fejléc-javítás nélkül, MIT, jó ötletforrás a UI-hoz), H.264 Datamosh Web Tool (NAL-szint), TD_RealMosh (újrakódolós), Glitchforge (zárt iOS app, MV-szerkesztést állít). Nyilvánosan MV-szintű, szabályos H.264-szerkesztést csak mi csinálunk.

## Javasolt fázisok

1. **Alap:** import (`prep`) progresszjelzővel, frame-szintű timeline I/P jelöléssel, törlés/duplikálás/áthelyezés üres helyekkel, Render gomb, előnézet léptetéssel, export.
2. **Szerkesztés:** effektsávok, beállítópanel, undo/redo, gyorsbillentyűk, projektfájl.
3. **Összehasonlítás:** variánsok, wipe/villogtatás, 2×2 paraméterrács.
4. **Extrák:** MV-overlay, mozgásgörbe, gyógyulásjelző, rámpák, presetek, hang.

Későbbi, nagyobb tételek (mind megoldható, de sok munka):

1. B-frame támogatás frame-szinten
2. CABAC makroblokk-szerkesztés
3. MV-effektek B-frame-en

## Tesztanyag a gépen

- `vidi1.mp4` (720p), `vidi2.mp4` (1080p), mindkettő a projekt gyökerében.
- `out/clips_ff.264`: a két klip CAVLC-vel összefűzve, 720p, 1209 frame, a vágás a 696. frame-nél.
- `out/v2_1080_cavlc.264`: vidi2 1080p CAVLC.
- `target/matrix/`: a roundtrip-mátrix streamjei.

## Munkastílus a felhasználóval

- Magyarul kommunikál; a válaszok legyenek magyarul.
- Ha csak kérdez, **ne kezdj munkába, amíg nem mondja, hogy mehet**.
- Döntések előtt röviden vázold a lehetőségeket ajánlással.
- A tesztek és referencia-kimenetek a regressziós kapuk: soha ne gyengítsd őket csak azért, hogy átmenjen valami.
