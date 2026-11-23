# Community plugins

Drop a plugin's C source (`*.c`) in this folder and GitHub Actions will compile it
into an aarch64 `.so` you can install on the stick — **no toolchain on your PC
needed**.

## How to get a compiled `.so` without installing anything

1. **Fork** this repository.
2. Add your plugin here, e.g. `plugins-comunidad/mi-plugin.c`. Start from
   [`../plugin-ejemplo/hola.c`](../plugin-ejemplo/hola.c) and include the ABI with
   `#include "abi.h"`.
3. Commit and push (or open a Pull Request). The **Compilar plugins** workflow runs
   automatically.
4. Open the **Actions** tab → your run → download the **`plugins-so`** artifact. It
   contains your `.so`.
5. On the stick, go to the panel → **Plugins → Subir** and upload the `.so`.

You can also trigger the workflow by hand from the **Actions** tab
("Run workflow").

## Writing the plugin

The full contract is in [`../plugins/abi.h`](../plugins/abi.h) and a how-to is in
[`../plugins/README.md`](../plugins/README.md).

## Security

A plugin runs **as root inside stickd, with no sandbox**. Only install `.so` files
you built or whose source you've read. A `.so` compiled here is exactly the source
in the matching `.c` — check it before trusting one.
