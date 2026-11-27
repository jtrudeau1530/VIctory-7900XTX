# VK_LAYER_VICTORY_placed

An implicit Vulkan layer for the Linux host, needed by `VICTORY_PORT_PLACED=1`
under Wine and vkd3d-proton. It enables `VK_KHR_map_memory2` and
`VK_EXT_map_memory_placed` (with `memoryMapPlaced` and `memoryUnmapReserve`) on
every device and raises the instance API version to 1.4, so that vkd3d-proton's
device can map a heap's memory at an address of the title's. Everything else
passes straight through. It is active only with `ENABLE_VICTORY_PLACED=1`.

Build (needs the Vulkan headers):

```sh
gcc -O2 -Wall -Wextra -Wno-unused-parameter -shared -fPIC -fvisibility=hidden \
    -o libVkLayer_victory_placed.so victory_placed_layer.c -lpthread
```

Install the manifest, with `@LIBRARY_PATH@` replaced by the absolute path of the
built `.so`:

```sh
mkdir -p ~/.local/share/vulkan/implicit_layer.d
sed "s#@LIBRARY_PATH@#$PWD/libVkLayer_victory_placed.so#" VkLayer_victory_placed.json.in \
    > ~/.local/share/vulkan/implicit_layer.d/VkLayer_victory_placed.json
```

Then run with `ENABLE_VICTORY_PLACED=1 VICTORY_PORT_CONSTANTS=1 VICTORY_PORT_PLACED=1`.
`VICTORY_PLACED_LOG=1` logs what the layer does to stderr; `DISABLE_VICTORY_PLACED=1`
turns it off.
