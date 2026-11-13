# Backups

This machine takes a snapshot every night with restic. The owner runs and
restores them from berth; you do not need to do anything for them to happen.

## What is in a snapshot

- Your home, `{{ devbox_home }}`, and everything under `/srv/agent`.
- The machine's own description: `/etc/devbox`, `/etc/berth`, `/var/lib/devbox`.

Left out, because it can be made again: caches, downloaded dependencies such as
`node_modules`, build output such as `target/debug` and `target/release`, and
installed runtimes. The full list is `/etc/berth-backup.exclude`, plus what the
files in `/etc/berth-backup.exclude.d` add for the tools on this machine.

So keep work you want to survive under your home or `/srv/agent`, and do not
keep the only copy of anything in a folder with one of those names.

## Looking at it

    cat /var/lib/berth-backup/last        # when the last snapshot was taken

Taking, listing and restoring snapshots needs root: `/usr/local/sbin/berth-backup`.
A restore writes over files in place. Leave it to the owner unless you were asked.

The repository password is `~/.config/berth-backup/password` and the rclone
configuration `~/.config/rclone/rclone.conf`. Both are secrets: never print,
copy or commit them, and do not change them, or the snapshots become unreadable.
