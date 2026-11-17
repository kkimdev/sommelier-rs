# Vendored Wayland protocols

`sommelier/build.rs` generates the proxy's protocol code from these XML
files, which come from the upstream projects below. To update one, replace
it with a newer upstream copy and record that revision here.

The upstream revision is listed where a byte-identical upstream release was
found. "Unknown" means no exact match was found; the file is as vendored in
the listed commit of this repository.

| File | Upstream | Revision | Vendored in |
| --- | --- | --- | --- |
| `wayland.xml` | [wayland](https://gitlab.freedesktop.org/wayland/wayland) `protocol/wayland.xml` | Unknown (includes `wl_fixes` and `wl_seat` v10, so wayland 1.24 or later) | `16463fc` |
| `xdg-shell.xml` | [wayland-protocols](https://gitlab.freedesktop.org/wayland/wayland-protocols) `stable/xdg-shell/` | Unknown (`xdg_wm_base` v7) | `16463fc` |
| `linux-dmabuf-v1.xml` | wayland-protocols `stable/linux-dmabuf/` | wayland-protocols 1.45 | `16463fc` |
| `viewporter.xml` | wayland-protocols `stable/viewporter/` | wayland-protocols 1.45 | `16463fc` |
| `fractional-scale-v1.xml` | wayland-protocols `staging/fractional-scale/` | wayland-protocols 1.45 | `16463fc` |
| `xdg-decoration-unstable-v1.xml` | wayland-protocols `unstable/xdg-decoration/` | wayland-protocols 1.45 | `16463fc` |
| `xdg-output-unstable-v1.xml` | wayland-protocols `unstable/xdg-output/` | wayland-protocols 1.45, unchanged through 1.49 (`zxdg_output_manager_v1` v3) | The `zxdg_output_manager_v1` passthrough |
| `text-input-unstable-v3.xml` | wayland-protocols `unstable/text-input/` | wayland-protocols 1.45 | `16463fc` |
| `text-input-unstable-v1.xml` | wayland-protocols `unstable/text-input/` | Unknown (predates the `bitfield` and `enum` attributes) | `16463fc` |
| `keyboard-extension-unstable-v1.xml` | Chromium Exo (`zcr_keyboard_extension_v1`) | Unknown (v2) | `16463fc` |
| `text-input-extension-unstable-v1.xml` | Chromium Exo (`zcr_text_input_extension_v1`) | Unknown (v11) | `16463fc` |
| `aura-shell.xml` | Chromium Exo (`zaura_shell`) | Unknown (v38) | `439a872` |
| `gtk-shell.xml` | Subset of GTK's `gtk_shell1`, as ChromiumOS Sommelier carries it | Unknown (v1) | `600fdf7` |

ChromiumOS Sommelier carries copies of the Chromium and GTK files in
[`vm_tools/sommelier/protocol/`](https://chromium.googlesource.com/chromiumos/platform2/+/main/vm_tools/sommelier/protocol/).
