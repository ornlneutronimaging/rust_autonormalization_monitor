# rust_autonormalization_monitor

Desktop GUI (Rust / egui) to drive the VENUS auto normalization: pick an
experiment and a normalization configuration, then either normalize every
upcoming run automatically or check a specific list of runs.

Single view, top to bottom:

1. **Experiment (IPTS)** — dropdown of the accessible `/SNS/VENUS/IPTS-*`
   folders (with a type-to-filter box) plus a manual entry field. Everything
   below is disabled until an IPTS is selected.
2. **Normalization configuration** — dropdown of the
   `<IPTS>/shared/autoreduce/configs/*.h5` files (newest first; hover for
   the full path), a **📂 Browse…** button to pick a configuration file
   from anywhere (the native file dialog opens in `<IPTS>/shared`), and a
   **👁 Preview** button that opens the selected file in the
   rust_nexus_viewer. A button launches the marimo
   **Normalization TOF at VENUS** notebook to create/edit a configuration: the notebook is
   provisioned into `<IPTS>/shared/notebooks/imaging_marimo_<user>/` and
   started from there, so it opens directly on the selected IPTS.
3. **What to normalize** —
   - the **Auto normalization ON/OFF** button: turning it ON registers the
     selected IPTS + configuration file in the shared `autoreduction.cfg`
     and sets its `activate` flag, so every upcoming run gets normalized;
     turning it OFF only clears the flag;
   - or a **list of runs** (e.g. `23615-23620, 23642`).
4. **Rolling combine & compare (NeuNorm)** — **opt-in**: the section
     heading is a checkbox (saved as the `rolling_combine` flag of the
     shared `autoreduction.cfg`) that makes the windows part of the auto
     normalization. Unchecked (the default for everybody), the auto
     normalization only normalizes each run on its own and the whole
     section is hidden. The windows (default last 5 / 15 /
     30 min of acquisition time, editable): each window collects the runs
     whose acquisition (NeXus `end_time`) ended within its last N minutes,
     anchored at the newest run considered. The runs of each window are
     normalized **together** through NeuNorm, via the VENUS workflow-runner
     script (`rust_workflow_runner/scripts/normalize_tof.py`, marimo pixi
     python) — samples are the runs' corrected folders, open beams come
     from the selected configuration file. Output:
     `<IPTS>/shared/autoreduce/normalized/rolling/anchor_<run>/last_<N>min`
     (staged in `.partial`, promoted on success). Once opted in, in
     **live mode** (no run list) the three normalizations fire
     automatically each time a new NeXus shows up (auto normalization ON +
     config selected). With a run
     list and auto normalization OFF the windows look at those runs only,
     launched by hand (**▶ Normalize windows now**); with auto
     normalization ON (hybrid mode) every run landing after the newest
     listed one joins the list automatically and the normalizations fire
     on each new NeXus. **👁 view** opens one window's folder in
     the rust_tiff_viewer; **👁 Compare all 3** (enabled once every window is
     normalized) opens a SINGLE viewer session with the three stacks side by
     side (`--compare`: shared colorscale, regions mirrored, one profile
     curve per stack — images only for now).
     Windows log into `rolling/anchor_<run>/logs/last_<N>min.log`.
5. **Runs in use table** — lists the runs the windows use (the manual
   list, or — in live mode — every run the widest window has covered since
   the session started: a new run slides the windows, never drops a row,
   so a normalization in progress stays visible). When auto normalization is
   ON, the first row is the **upcoming run** (highest run in
   `<IPTS>/nexus` + 1, refreshed automatically) that will be normalized
   next. Each row has a **👁 Preview** button opening the run's corrected
   folder in the rust_tiff_viewer, a **Normalized** column, and a
   **✖ reject / ↩ restore** toggle:
   a rejected run stays listed (crossed out) but leaves the windows and
   their normalizations. The span of runs listed is anchored at the
   newest run whether rejected or not, so rejecting the latest run(s)
   does not pull older runs back into the table: once everything in the
   span is rejected the windows are empty and the app simply waits for
   the next run.
   With auto normalization ON, every run that lands from then on is
   normalized **on its own** by this app: the row shows **⏳ waiting**
   until the run's corrected folder is complete (the Corrected column
   turns green — a folder the reduction is still filling shows an amber
   ⏳: the images land first, the `*_Spectra.txt` / `summary.json`
   sidecars last, and only then is the data used), then NeuNorm runs with the selected configuration file whose
   sample is replaced by that run (open beams and settings unchanged) —
   a progress bar fills in the Normalized column, spanning the WHOLE
   workflow ("stage k/n", hover for the list of stages, ✔ for the done
   ones; the script announces its stages up front, and the fill of
   unmeasured pixels — silent inside NeuNorm — is reported as its own
   stage), and once done a
   **👁** icon opens the result in the rust_tiff_viewer and a **📂** icon
   opens the folder in the file manager (**↻** retries a failed run).
   A **📋** button on any normalized row (running, done or failed) opens an
   output panel under the table with the job's output as it streams —
   the script's own messages, the NeuNorm log lines, the runner's steps.
   The job runs exactly like the workflow runner's: a result already
   there is never redone, the inputs are pre-cropped on disk when the
   configuration has a crop region, and the script's output is streamed
   into `Run_<run>/logs/normalization.log` (a failed row's **📄** button
   opens it; the hover shows the last lines).
   Output follows the workflow runner's layout under the configuration's
   output folder: `<output folder>/Run_<run>/normalization` (under
   `<IPTS>/shared/autoreduce/normalized` when the configuration names no
   output folder). A result already there — from the workflow runner or a
   previous session — is shown as done and never redone. Every run that
   shows up as "(next)" — and any run still in flight when the app started
   watching (NeXus there, corrected data not yet) — is normalized
   automatically, no click needed. Runs that already had their corrected
   data when auto normalization was turned on (older runs, typically the
   ones typed into the run list) are left alone: when their result is not
   found in that output folder they get a **▶ normalize** button that runs
   the same per-run normalization on demand. A **📈 Timeline** tab next to the
   table shows when each run was acquired (start → end bar per run, run
   number and duration on hover, rejected runs grayed/struck) with the
   5/15/30 min window coverage bands on top, on a shared axis in minutes
   relative to the latest run. For each run, one column per file kind
   with a
   ✔ (found) / ✘ (not there yet) icon; hovering an icon shows the full
   path. Locations inside the IPTS folder:
   - **NeXus**: `nexus/VENUS_<run>.nxs.h5`
   - **Raw**: folder named `*_Run_<run>_*` under `images/`
   - **Corrected**: same pattern under `shared/autoreduce/images/`

The shared configuration is `/SNS/VENUS/shared/autoreduction/autoreduction.cfg`
(the file the notebook writes), falling back to the legacy
`/SNS/VENUS/shared/autoreduce/autoreduction.cfg` when only that one exists.
It is re-read on the auto-refresh period (default 5 s, adjustable in the
top bar) so the display always reflects changes made by other tools. The
monitor adds its own `rolling_combine` key to the notebook's schema; the
notebook rewrites the file without it when a configuration is registered,
which resets the opt-in to unchecked.

## Run

From a graphical session (e.g. ThinLinc):

```bash
./launch_autonormalization_monitor.sh
```

The script rebuilds the release binary automatically when the sources have
changed.

## Development

```bash
cargo build --release   # build
cargo test              # config read/write + run list/file discovery tests
```

Uses the shared VENUS rust application template: ORNL "Coefficient" design
tokens (`src/theme.rs`), light/dark toggle shared by all the VENUS rust
tools, and the branded green header with the neutron imaging logo.
