# rust_autonormalization_monitor

Desktop GUI (Rust / egui) that monitors the state of the VENUS auto
normalization pipeline.

The application shows the `activate` flag of
`/SNS/VENUS/shared/autoreduction/autoreduction.cfg` as a large **ON / OFF**
button at the top of the window, re-reading the file every 2 seconds so it
always reflects changes made by other tools.

The state is read-only for regular users. An admin can unlock editing by
entering the admin password (only its SHA-256 hash is stored in the code);
once unlocked, clicking the ON/OFF button flips the flag and writes it back
to the configuration file, updating the `last_modified` /
`last_modified_by` bookkeeping fields.

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
cargo test              # config file read/write unit tests
```

Uses the shared VENUS rust application template: ORNL "Coefficient" design
tokens (`src/theme.rs`) and the branded green header with the neutron
imaging logo.
