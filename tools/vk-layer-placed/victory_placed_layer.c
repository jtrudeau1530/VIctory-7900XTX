/* VK_LAYER_VICTORY_placed -- implicit Vulkan layer (Linux host side).
 *
 * Purpose: let a Windows app running under Wine + vkd3d-proton call
 * vkMapMemory2 with VK_MEMORY_MAP_PLACED_BIT_EXT on vkd3d's device.
 *  - vkCreateInstance: apiVersion raised to >= 1.4 (app's kept if higher; see
 *    below why 1.3 is not enough under VK_KHR_maintenance5).
 *  - vkCreateDevice: VK_KHR_map_memory2 + VK_EXT_map_memory_placed appended
 *    (when the GPU has them), VkPhysicalDeviceMapMemoryPlacedFeaturesEXT
 *    {memoryMapPlaced, memoryUnmapReserve} chained (or forced on if present).
 *  - vkGetDeviceProcAddr: vkMapMemory2 / vkMapMemory2KHR / vkUnmapMemory2 /
 *    vkUnmapMemory2KHR resolve to the driver's (core name falls back to the
 *    KHR alias), so winevulkan's device->p_vkMapMemory2 (and _KHR) are set.
 * Everything else is passed straight through (the layer is not in any
 * other call path: it returns the next layer's pointers directly).
 * Activated only with ENABLE_VICTORY_PLACED=1 (manifest enable_environment).
 */
#include <vulkan/vulkan.h>
#include <vulkan/vk_layer.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define EXPORT __attribute__((visibility("default")))
#define LOG(...) do { if (getenv("VICTORY_PLACED_LOG")) { fprintf(stderr, "[victory_placed] " __VA_ARGS__); } } while (0)

typedef struct { void *key; PFN_vkGetInstanceProcAddr gipa; VkInstance inst;
                 PFN_vkEnumerateDeviceExtensionProperties enum_ext; } inst_ent;
typedef struct { void *key; PFN_vkGetDeviceProcAddr gdpa; VkDevice dev; PFN_vkDestroyDevice destroy; } dev_ent;

#define MAXE 64
static inst_ent insts[MAXE];
static dev_ent devs[MAXE];
static pthread_mutex_t mtx = PTHREAD_MUTEX_INITIALIZER;

static void *dkey(const void *h) { return *(void *const *)h; }

static inst_ent *find_inst(void *key) {
    for (int i = 0; i < MAXE; i++) if (insts[i].key == key) return &insts[i];
    return NULL;
}
static dev_ent *find_dev(void *key) {
    for (int i = 0; i < MAXE; i++) if (devs[i].key == key) return &devs[i];
    return NULL;
}

static VkLayerInstanceCreateInfo *inst_chain(const VkInstanceCreateInfo *ci) {
    VkLayerInstanceCreateInfo *c = (VkLayerInstanceCreateInfo *)ci->pNext;
    while (c && !(c->sType == VK_STRUCTURE_TYPE_LOADER_INSTANCE_CREATE_INFO && c->function == VK_LAYER_LINK_INFO))
        c = (VkLayerInstanceCreateInfo *)c->pNext;
    return c;
}
static VkLayerDeviceCreateInfo *dev_chain(const VkDeviceCreateInfo *ci) {
    VkLayerDeviceCreateInfo *c = (VkLayerDeviceCreateInfo *)ci->pNext;
    while (c && !(c->sType == VK_STRUCTURE_TYPE_LOADER_DEVICE_CREATE_INFO && c->function == VK_LAYER_LINK_INFO))
        c = (VkLayerDeviceCreateInfo *)c->pNext;
    return c;
}

static VKAPI_ATTR VkResult VKAPI_CALL layer_CreateInstance(const VkInstanceCreateInfo *ci,
        const VkAllocationCallbacks *alloc, VkInstance *out) {
    VkLayerInstanceCreateInfo *chain = inst_chain(ci);
    if (!chain || !chain->u.pLayerInfo) return VK_ERROR_INITIALIZATION_FAILED;
    PFN_vkGetInstanceProcAddr gipa = chain->u.pLayerInfo->pfnNextGetInstanceProcAddr;
    chain->u.pLayerInfo = chain->u.pLayerInfo->pNext;
    PFN_vkCreateInstance next_create = (PFN_vkCreateInstance)gipa(VK_NULL_HANDLE, "vkCreateInstance");

    VkInstanceCreateInfo nci = *ci;
    VkApplicationInfo app = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO };
    if (ci->pApplicationInfo) app = *ci->pApplicationInfo;
    uint32_t old = app.apiVersion ? app.apiVersion : VK_API_VERSION_1_0;
    /* 1.4 by default: vkd3d-proton enables VK_KHR_maintenance5, under which the
     * loader's vkGetDeviceProcAddr returns NULL for core-1.4 names (vkMapMemory2,
     * vkUnmapMemory2) when the instance is < 1.4, and winevulkan's core thunks
     * call exactly those (device->p_vkMapMemory2). VICTORY_PLACED_API=13 keeps 1.3. */
    const char *api = getenv("VICTORY_PLACED_API");
    uint32_t floor_ = (api && !strcmp(api, "13")) ? VK_API_VERSION_1_3 : VK_API_VERSION_1_4;
    if (old < floor_) app.apiVersion = floor_;
    nci.pApplicationInfo = &app;
    LOG("vkCreateInstance apiVersion %u.%u -> %u.%u\n", VK_API_VERSION_MAJOR(old), VK_API_VERSION_MINOR(old),
        VK_API_VERSION_MAJOR(app.apiVersion), VK_API_VERSION_MINOR(app.apiVersion));

    VkResult r = next_create(&nci, alloc, out);
    if (r != VK_SUCCESS) return r;
    pthread_mutex_lock(&mtx);
    inst_ent *e = find_inst(NULL);
    if (e) {
        e->key = dkey(*out); e->gipa = gipa; e->inst = *out;
        e->enum_ext = (PFN_vkEnumerateDeviceExtensionProperties)gipa(*out, "vkEnumerateDeviceExtensionProperties");
    }
    pthread_mutex_unlock(&mtx);
    return VK_SUCCESS;
}

static VKAPI_ATTR void VKAPI_CALL layer_DestroyInstance(VkInstance inst, const VkAllocationCallbacks *alloc) {
    pthread_mutex_lock(&mtx);
    inst_ent *e = find_inst(dkey(inst));
    PFN_vkDestroyInstance d = e ? (PFN_vkDestroyInstance)e->gipa(inst, "vkDestroyInstance") : NULL;
    if (e) memset(e, 0, sizeof *e);
    pthread_mutex_unlock(&mtx);
    if (d) d(inst, alloc);
}

static int has_ext(const VkExtensionProperties *p, uint32_t n, const char *name) {
    for (uint32_t i = 0; i < n; i++) if (!strcmp(p[i].extensionName, name)) return 1;
    return 0;
}

static VKAPI_ATTR VkResult VKAPI_CALL layer_CreateDevice(VkPhysicalDevice pd, const VkDeviceCreateInfo *ci,
        const VkAllocationCallbacks *alloc, VkDevice *out) {
    VkLayerDeviceCreateInfo *chain = dev_chain(ci);
    if (!chain || !chain->u.pLayerInfo) return VK_ERROR_INITIALIZATION_FAILED;
    PFN_vkGetInstanceProcAddr gipa = chain->u.pLayerInfo->pfnNextGetInstanceProcAddr;
    PFN_vkGetDeviceProcAddr gdpa = chain->u.pLayerInfo->pfnNextGetDeviceProcAddr;
    chain->u.pLayerInfo = chain->u.pLayerInfo->pNext;

    pthread_mutex_lock(&mtx);
    inst_ent *ie = find_inst(dkey(pd));
    VkInstance inst = ie ? ie->inst : VK_NULL_HANDLE;
    PFN_vkEnumerateDeviceExtensionProperties enum_ext = ie ? ie->enum_ext : NULL;
    pthread_mutex_unlock(&mtx);
    PFN_vkCreateDevice next_create = (PFN_vkCreateDevice)gipa(inst, "vkCreateDevice");
    if (!next_create) return VK_ERROR_INITIALIZATION_FAILED;

    /* What the GPU offers. */
    uint32_t navail = 0; VkExtensionProperties *avail = NULL;
    if (enum_ext && enum_ext(pd, NULL, &navail, NULL) == VK_SUCCESS && navail) {
        avail = calloc(navail, sizeof *avail);
        if (enum_ext(pd, NULL, &navail, avail) != VK_SUCCESS) navail = 0;
    }
    static const char *want[] = { VK_KHR_MAP_MEMORY_2_EXTENSION_NAME, VK_EXT_MAP_MEMORY_PLACED_EXTENSION_NAME };
    int can = avail && has_ext(avail, navail, want[0]) && has_ext(avail, navail, want[1]);
    free(avail);

    VkDeviceCreateInfo nci = *ci;
    const char **exts = NULL;
    VkPhysicalDeviceMapMemoryPlacedFeaturesEXT feat = {
        .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_MAP_MEMORY_PLACED_FEATURES_EXT,
        .memoryMapPlaced = VK_TRUE, .memoryMapRangePlaced = VK_FALSE, .memoryUnmapReserve = VK_TRUE };
    int chained = 0;
    if (can) {
        PFN_vkGetPhysicalDeviceFeatures2 query =
            (PFN_vkGetPhysicalDeviceFeatures2)gipa(inst, "vkGetPhysicalDeviceFeatures2");
        VkPhysicalDeviceMapMemoryPlacedFeaturesEXT supported = {
            .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_MAP_MEMORY_PLACED_FEATURES_EXT };
        VkPhysicalDeviceFeatures2 features = {
            .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2, .pNext = &supported };
        if (query) query(pd, &features);
        can = supported.memoryMapPlaced && supported.memoryUnmapReserve;
        for (const VkBaseInStructure *s = ci->pNext; s; s = s->pNext) {
            if (s->sType != VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_MAP_MEMORY_PLACED_FEATURES_EXT) continue;
            const VkPhysicalDeviceMapMemoryPlacedFeaturesEXT *requested =
                (const VkPhysicalDeviceMapMemoryPlacedFeaturesEXT *)s;
            can = can && requested->memoryMapPlaced && requested->memoryUnmapReserve;
            chained = 1;
        }
    }
    if (can) {
        if (ci->enabledExtensionCount > UINT32_MAX - 2) return VK_ERROR_OUT_OF_HOST_MEMORY;
        exts = calloc(ci->enabledExtensionCount + 2, sizeof *exts);
        if (!exts) return VK_ERROR_OUT_OF_HOST_MEMORY;
        uint32_t n = 0;
        for (uint32_t i = 0; i < ci->enabledExtensionCount; i++) exts[n++] = ci->ppEnabledExtensionNames[i];
        for (int w = 0; w < 2; w++) {
            int found = 0;
            for (uint32_t i = 0; i < n; i++) if (!strcmp(exts[i], want[w])) found = 1;
            if (!found) { exts[n++] = want[w]; LOG("vkCreateDevice: +%s\n", want[w]); }
        }
        nci.enabledExtensionCount = n;
        nci.ppEnabledExtensionNames = exts;
        /* Keep the application's feature records unchanged. */
        if (!chained) { feat.pNext = (void *)nci.pNext; nci.pNext = &feat; }
    } else {
        LOG("vkCreateDevice: placed mapping features unavailable or unrequested; passing through\n");
    }

    VkResult r = next_create(pd, &nci, alloc, out);
    free(exts);
    LOG("vkCreateDevice -> %d\n", r);
    if (r != VK_SUCCESS) return r;
    pthread_mutex_lock(&mtx);
    dev_ent *de = find_dev(NULL);
    if (de) { de->key = dkey(*out); de->gdpa = gdpa; de->dev = *out;
              de->destroy = (PFN_vkDestroyDevice)gdpa(*out, "vkDestroyDevice"); }
    pthread_mutex_unlock(&mtx);
    return VK_SUCCESS;
}

static VKAPI_ATTR void VKAPI_CALL layer_DestroyDevice(VkDevice dev, const VkAllocationCallbacks *alloc) {
    pthread_mutex_lock(&mtx);
    dev_ent *e = find_dev(dkey(dev));
    PFN_vkDestroyDevice d = e ? e->destroy : NULL;
    if (e) memset(e, 0, sizeof *e);
    pthread_mutex_unlock(&mtx);
    if (d) d(dev, alloc);
}

EXPORT VKAPI_ATTR PFN_vkVoidFunction VKAPI_CALL victory_placed_GetDeviceProcAddr(VkDevice dev, const char *name);
EXPORT VKAPI_ATTR PFN_vkVoidFunction VKAPI_CALL victory_placed_GetInstanceProcAddr(VkInstance inst, const char *name);

EXPORT VKAPI_ATTR PFN_vkVoidFunction VKAPI_CALL victory_placed_GetDeviceProcAddr(VkDevice dev, const char *name) {
    if (!strcmp(name, "vkGetDeviceProcAddr")) return (PFN_vkVoidFunction)victory_placed_GetDeviceProcAddr;
    if (!strcmp(name, "vkDestroyDevice")) return (PFN_vkVoidFunction)layer_DestroyDevice;
    pthread_mutex_lock(&mtx);
    dev_ent *e = dev ? find_dev(dkey(dev)) : NULL;
    PFN_vkGetDeviceProcAddr gdpa = e ? e->gdpa : NULL;
    pthread_mutex_unlock(&mtx);
    if (!gdpa) return NULL;
    PFN_vkVoidFunction f = gdpa(dev, name);
    /* map_memory2 family: core name falls back to the KHR alias and vice versa. */
    if (!f && !strncmp(name, "vk", 2) && (strstr(name, "apMemory2"))) {
        char alt[64];
        size_t l = strlen(name);
        if (l > 3 && !strcmp(name + l - 3, "KHR")) { snprintf(alt, sizeof alt, "%.*s", (int)(l - 3), name); }
        else snprintf(alt, sizeof alt, "%sKHR", name);
        f = gdpa(dev, alt);
        LOG("GDPA %s -> NULL, alias %s -> %p\n", name, alt, (void *)f);
    } else if (strstr(name, "apMemory2")) {
        LOG("GDPA %s -> %p\n", name, (void *)f);
    }
    return f;
}

EXPORT VKAPI_ATTR PFN_vkVoidFunction VKAPI_CALL victory_placed_GetInstanceProcAddr(VkInstance inst, const char *name) {
    if (!strcmp(name, "vkGetInstanceProcAddr")) return (PFN_vkVoidFunction)victory_placed_GetInstanceProcAddr;
    if (!strcmp(name, "vkGetDeviceProcAddr")) return (PFN_vkVoidFunction)victory_placed_GetDeviceProcAddr;
    if (!strcmp(name, "vkCreateInstance")) return (PFN_vkVoidFunction)layer_CreateInstance;
    if (!strcmp(name, "vkDestroyInstance")) return (PFN_vkVoidFunction)layer_DestroyInstance;
    if (!strcmp(name, "vkCreateDevice")) return (PFN_vkVoidFunction)layer_CreateDevice;
    if (!strcmp(name, "vkDestroyDevice")) return (PFN_vkVoidFunction)layer_DestroyDevice;
    if (!inst) return NULL;
    pthread_mutex_lock(&mtx);
    inst_ent *e = find_inst(dkey(inst));
    PFN_vkGetInstanceProcAddr gipa = e ? e->gipa : NULL;
    pthread_mutex_unlock(&mtx);
    return gipa ? gipa(inst, name) : NULL;
}

EXPORT VKAPI_ATTR VkResult VKAPI_CALL victory_placed_NegotiateLoaderLayerInterfaceVersion(VkNegotiateLayerInterface *v) {
    if (v->loaderLayerInterfaceVersion > 2) v->loaderLayerInterfaceVersion = 2;
    v->pfnGetInstanceProcAddr = victory_placed_GetInstanceProcAddr;
    v->pfnGetDeviceProcAddr = victory_placed_GetDeviceProcAddr;
    v->pfnGetPhysicalDeviceProcAddr = NULL;
    return VK_SUCCESS;
}
