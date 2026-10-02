# Packaging notes

Releases ship as an AppImage (built by `appimage/build-appimage.sh` in the AppImage workflow) and a cargo-dist tarball and shell installer; a `.deb` can be built by hand with `cargo deb -p f9-talk`.

## udev rule for `/dev/uinput`

By default `/dev/uinput` is mode `0600 root:root`, so the F9 Talk binary
can't write to it as a non-root user even if the user is in the `input`
group. We ship `debian/udev/99-f9-talk.rules` which changes it to
`0660 root:input`.

`sudo f9-talk install --system` (or the `.deb` postinst) installs the rule,
reloads udev and adds you to the `input` group. For a source build you can
do the same by hand:

```sh
sudo cp packaging/debian/udev/99-f9-talk.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules
sudo udevadm trigger /dev/uinput
ls -l /dev/uinput   # expect: crw-rw---- 1 root input ...
sudo usermod -aG input "$USER"
# then log out and back in once
```

## Why uinput (not xdotool / wtype / ydotool)

- `xdotool` is X11-only - broken on native Wayland windows.
- `wtype` works on Wayland but is blocked on GNOME (the compositor
  doesn't implement `virtual-keyboard-unstable-v1`).
- `ydotool` does what we need but adds a runtime daemon dep.
- Direct `/dev/uinput` write injects key events at the kernel layer:
  works on X11 + Wayland identically, no extra processes. On Wayland the
  typer puts the text on the clipboard with `wl-copy` and sends one
  Ctrl+Shift+V through uinput (no dropped characters); `wtype` is the
  fallback when `wl-clipboard` is missing.
