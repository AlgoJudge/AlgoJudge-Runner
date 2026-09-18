## What changes and why

<!-- One subject per pull request. If it closes an issue, write "Closes #123". -->

## How it was tested

<!-- The commands you ran, and the submissions you judged by hand. -->

## Checklist

- [ ] `./x gate` passes. It runs formatting, Clippy, the build, and the unit tests the way CI does.
- [ ] A change to isolation or limits is covered by the integration or adversarial suite. CI runs both under the cgroupfs and systemd drivers.
- [ ] The Server–Runner protocol is unchanged, or the change was agreed in an issue first.
- [ ] A new configuration key is in `.env.example`.
- [ ] No secrets, credentials, or `.env` files are committed.
