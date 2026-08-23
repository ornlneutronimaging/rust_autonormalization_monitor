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
   the full path), with a **👁 Preview** button that opens the selected
   file in the rust_nexus_viewer. A button launches the marimo
   **Normalization TOF at VENUS** notebook to create/edit a configuration: the notebook is
   provisioned into `<IPTS>/shared/notebooks/imaging_marimo_<user>/` and
   started from there, so it opens directly on the selected IPTS.
3. **What to normalize** —
   - the **Auto normalization ON/OFF** button: turning it ON registers the
     selected IPTS + configuration file in the shared `autoreduction.cfg`
     and sets its `activate` flag, so every upcoming run gets normalized;
     turning it OFF only clears the flag;
   - or a **list of runs** (e.g. `23615-23620, 23642`).
4. **Rolling combine & compare (NeuNorm)** — the windows (default last 5 / 15 /
     30 min of acquisition time, editable): each window collects the runs
     whose acquisition (NeXus `end_time`) ended within its last N minutes,
     anchored at the newest run considered. The runs of each window are
     normalized **together** through NeuNorm, via the VENUS workflow-runner
     script (`rust_workflow_runner/scripts/normalize_tof.py`, marimo pixi
     python) — samples are the runs' corrected folders, open beams come
     from the selected configuration file. Output:
     `<IPTS>/shared/autoreduce/normalized/rolling/anchor_<run>/last_<N>min`
     (staged in `.partial`, promoted on success). In **live mode** (no run
     list) the three normalizations fire automatically each time a new
     NeXus shows up (auto normalization ON + config selected). With a run
     list and auto normalization OFF the windows look at those runs only,
     launched by hand (**▶ Normalize windows now**); with auto
     normalization ON (hybrid mode) every run landing after the newest
     listed one joins the list automatically and the normalizations fire
     on each new NeXus. **👁 view** opens one window's folder in
     the rust_tiff_viewer; **👁 Compare all 3** (enabled once every window is
     normalized) opens a SINGLE viewer session with the three stacks side by
     side (`--compare`: shared colorscale, regions mirrored, one profile
     curve per stack — images only for now).
     Configurations with a crop region are not supported yet.
5. **Runs in use table** — lists the runs the windows use (the manual
   list, or the widest window in live mode). When auto normalization is
   ON, the first row is the **upcoming run** (highest run in
   `<IPTS>/nexus` + 1, refreshed automatically) that will be normalized
   next. Each row has a **👁 Preview** button opening the run's corrected
   folder in the rust_tiff_viewer, and a **✖ reject / ↩ restore** toggle:
   a rejected run stays listed (crossed out) but leaves the windows and
   their normalizations — rejecting the newest run slides the window
   anchor back to the previous one. For each run, one column per file
   kind with a
   ✔ (found) / ✘ (not there yet) icon; hovering an icon shows the full
   path. Locations inside the IPTS folder:
   - **NeXus**: `nexus/VENUS_<run>.nxs.h5`
   - **Raw**: folder named `*_Run_<run>_*` under `images/`
   - **Corrected**: same pattern under `shared/autoreduce/images/`

The shared configuration is `/SNS/VENUS/shared/autoreduction/autoreduction.cfg`
(the file the notebook writes), falling back to the legacy
`/SNS/VENUS/shared/autoreduce/autoreduction.cfg` when only that one exists.
It is re-read on the auto-refresh period (default 5 s, adjustable in the
top bar) so the display always reflects changes made by other tools.

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
