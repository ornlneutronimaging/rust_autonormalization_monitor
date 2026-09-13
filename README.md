# rust_autonormalization_monitor

Desktop GUI (Rust / egui) to drive the VENUS auto normalization: pick an
experiment and a normalization configuration, then either normalize every
upcoming run automatically or check a specific list of runs.

Single view, top to bottom:

1. **Experiment (IPTS)** — dropdown of the accessible `/SNS/VENUS/IPTS-*`
   folders (with a type-to-filter box) plus a manual entry field. Everything
   below is disabled until an IPTS is selected.
2. **Normalization configuration** — dropdown of the `.h5` files in
   `<IPTS>/shared/autoreduce` and its `configs/` subfolder (newest first,
   the newest selected by default — at startup too, so the file the
   notebook saved last, or the `_ob_<run>` copy this app wrote last when
   switching open beams, is the one in use; hover for
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
   A footer under the table reminds where the normalized data lands (the
   configuration's output folder — `Run_<run>/normalization` in there) with
   a **📂 open folder** shortcut.
   With auto normalization ON, every run that lands from then on is
   normalized **on its own** by this app: the row shows **⏳ waiting**
   until the run's corrected folder is complete (the Corrected column
   turns green — a folder the reduction is still filling shows an amber
   ⏳: a folder is complete when its `*_Spectra.txt` is there and its
   number of data rows equals the number of frames — the `.fits` files
   minus the SummedImg one for a raw folder, the `.tif` files for a
   corrected one, which must also have its `summary.json`, written last),
   then NeuNorm runs with the selected configuration file whose
   sample is replaced by that run (open beams and settings unchanged) —
   a progress bar fills in the Normalized column, spanning the WHOLE
   workflow ("stage k/n", hover for the list of stages, ✔ for the done
   ones; the script announces its stages up front, and the fill of
   unmeasured pixels — silent inside NeuNorm — is reported as its own
   stage), and once done the
   cell reads **✔ normalization done** (hover: time and path) with a
   **👁 view** button opening the result in the rust_tiff_viewer, a
   **📂 folder** button jumping to the folder in the file manager and a
   **↻** re-run button (below). The
   viewer is launched with `--detector <name>` (the configuration's
   `detector` attribute, e.g. `tpx1`, else the single detector folder of
   `shared/autoreduce/images`), so Timepix stacks open transposed the right
   way even from a `Run_<run>/normalization` path that names no detector
   (**↻** retries a failed run). Before launching, the sample and every
   open-beam folder must hold the same number of images (NeuNorm divides
   the stacks frame by frame — the notebook's Data Quality Check refuses
   the same way); otherwise the row fails at once with the counts.
   **Alignment runs** are recognized as soon as their NeXus is there: the
   DAQ files them under `images/<detector>/alignment/…` and records that
   folder in the NeXus (`BL10:Exp:IM:ImageFilePath` log, last value); the
   autoreduction never corrects them. Such a row reads **alignment run —
   no normalization needed** (hover: the recorded folder), its Corrected
   cell is not checked, it gets no ▶ normalize button, is skipped by
   "normalize all missing" and never enters the windows (nor the timeline
   anchor — the bar is greyed and tagged "(alignment)" like a rejected
   run's).
   **Open-beam runs** are recognized the same way (filed under
   `images/<detector>/ob/…`): the autoreduction corrects them like any
   run, but they are normalization *inputs* — the row reads **open beam —
   no normalization needed**, is skipped by the automatic pass and by
   "normalize all missing", and never enters the windows (timeline tag
   "(open beam)"). Its ✖ reject only keeps it out of the banner below.
   An **Open beam** column names, for every row, the open-beam run(s) the
   normalization divides by: what the result actually used for a
   normalized run (recorded when the job is launched here, read back from
   the `--ob` arguments of `Run_<run>/logs/normalization.log` for a result
   found on disk — this tool and the workflow runner write the same log),
   the configuration's open beams, dimmed, for a run still to come (and
   for the "(next)" row). An open-beam row shows **✔ in use** when it is
   one of the configuration's open beams, **new** when it landed after
   them. When such new open beam(s) exist, a notice above the table —
   **🔆 New open beam run(s): 29962, 29963** — says so (and which are
   still waiting for their corrected data); it disappears once the
   configuration names them, an even newer open beam brings it back.
   **⇄ replace by…**, right after the Open beam cell of every sample row,
   is how the open beams get switched: it opens a window listing every
   corrected open-beam folder of the IPTS
   (`shared/autoreduce/images/<detector>/ob`, newest first, frame count
   of each, the run's own frame count recalled at the top and a mismatch
   flagged, a folder still being written cannot be ticked, the
   configuration's marked and the ones the run currently uses ticked) —
   tick any (1 or more), then either **Replace for this run** (that run
   only: it is normalized at once when its corrected data is complete —
   re-run when it already was, the previous result kept as
   `normalization.previous` — otherwise as soon as it is), or **Replace
   for this run and every upcoming run**: a copy of the selected
   configuration file with those folders as its `ob/folders` (everything
   else kept: sample, masks, settings, output folder; `ob/run_spec` and a
   `derived_from` root attribute say where it comes from) is written next
   to it as `<name>_ob_<run>_<run>.h5` and selected, so every run
   normalized from then on — per-run and windows alike — divides by the
   new open beams; with auto normalization ON the new file is registered
   in `autoreduction.cfg` at once. Runs already normalized are not redone.
   **Config** and **Output** columns name, for every row, the
   configuration file the normalization ran with and the base folder its
   result sits in (`<base>/Run_<run>/normalization`; hover for the full
   paths) — from the job log for a result found on disk, the selected
   configuration and its output folder, dimmed, for a run still to come.
   **✎ next to the Output cell** opens a window to type or browse another
   output folder for that run only: **Apply** keeps it (the automatic
   normalization and the ▶ normalize button use it; a result already
   there is looked up in the new folder), **Apply & normalize now / re-run
   now** runs it at once. The Open beam / Output cells of a run with its
   own open beams / output folder read in blue and the ⇄ / ✎ buttons are
   highlighted; these per-run choices are kept for the session.
   **↻ re-run** on every normalized row runs the normalization again with
   the row's current settings (in case a setting, an open beam or the
   configuration changed after all): the result already sitting where the
   new one goes is moved aside as `normalization.previous` (replacing an
   older one — the job never runs twice into the same folder, and the
   append-only job log keeps the history); a result elsewhere (other
   output folder) is left alone.
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
   the same per-run normalization on demand; **▶ normalize all missing**
   in the section heading queues every such run (newest first). The
   **parallel jobs** field next to it (default 4, up to 16) says how many
   per-run normalizations may run side by side — for the queue and for the
   automatic normalization alike: each one is its own NeuNorm python
   process, a burst of new runs is throttled to that many, the rest wait
   for a free slot (retried at every refresh). Selecting a different configuration file while auto
   normalization is ON re-registers it in `autoreduction.cfg` at once, so
   the autoreduction and this app agree on the open beams. Rows are listed
   newest first, right under the upcoming run. A **📈 Timeline** tab next to the
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
