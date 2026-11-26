# Describe DRM devices that are not on PCI, whose kernel bus ID is
# "platform:<name>", as platform devices; Mesa's EGL needs them to be
# described.  Do not return device nodes that do not exist.  Read a node's
# bus ID from dev.drm.N.busid, and find a node's other nodes (render, card)
# by that bus ID, not by number: on boards whose display (sysfbdrm) and GPU
# (msm, panthor) drivers are separate, the numbers do not match.
#
# From https://github.com/JamesKane/libdrm, branch freebsd-missing-node.
#
--- xf86drm.c.orig
+++ xf86drm.c
@@ -3390,6 +3390,18 @@ drm_public int drmCloseBufferHandle(int fd, uint32_t handle)
     return drmIoctl(fd, DRM_IOCTL_GEM_CLOSE, &args);
 }
 
+#ifdef __FreeBSD__
+/* The bus ID the kernel gives /dev/drm/<minor>, if it has one. */
+static int get_sysctl_minor_busid(int minor, char *busid, size_t len)
+{
+    char sysctl_name[32];
+    size_t size = len;
+
+    snprintf(sysctl_name, sizeof(sysctl_name), "dev.drm.%d.busid", minor);
+    return sysctlbyname(sysctl_name, busid, &size, NULL, 0) == 0 ? 0 : -1;
+}
+#endif
+
 static char *drmGetMinorNameForFD(int fd, int type)
 {
 #ifdef __linux__
@@ -3439,6 +3451,7 @@ static char *drmGetMinorNameForFD(int fd, int type)
     char dname[SPECNAMELEN];
     const char *mname;
     char name[SPECNAMELEN];
+    char busid[256], other[256];
     int id, maj, min, nodetype, i;
 
     if (fstat(fd, &sbuf))
@@ -3472,9 +3485,31 @@ static char *drmGetMinorNameForFD(int fd, int type)
         return (NULL);
 
     id = (int)strtol(&dname[i], NULL, 10);
+
+    /* The node of the requested type that belongs to the same device: the
+     * one with the same bus ID.  A device need not have every type, e.g.
+     * KMS-only drivers have no render node, and the node numbers of
+     * different types need not match when several drivers are loaded. */
+    if (get_sysctl_minor_busid(id, busid, sizeof(busid)) == 0) {
+        int base = drmGetMinorBase(type);
+
+        for (i = base; i < base + 64; i++) {
+            if (get_sysctl_minor_busid(i, other, sizeof(other)) != 0 ||
+                strcmp(busid, other) != 0)
+                continue;
+            snprintf(name, sizeof(name), DRM_DIR_NAME "/%s%d", mname, i);
+            if (stat(name, &sbuf) == 0)
+                return strdup(name);
+        }
+        return NULL;
+    }
+
+    /* Older kernels have no bus ID per node: guess by number. */
     id -= drmGetMinorBase(nodetype);
     snprintf(name, sizeof(name), DRM_DIR_NAME "/%s%d", mname,
          id + drmGetMinorBase(type));
+    if (stat(name, &sbuf) != 0)
+        return NULL;
 
     return strdup(name);
 #else
@@ -3599,6 +3634,63 @@ static int get_subsystem_type(const char *device_path)
 }
 #endif
 
+#ifdef __FreeBSD__
+/*
+ * The bus ID the kernel gives the device of a node: dev.drm.N.busid for
+ * /dev/drm/N.  Older kernels only have hw.dri.M.busid, one per device, where
+ * card N and renderD(128+N) are guessed to be hw.dri.N.
+ */
+static int get_sysctl_busid(int maj, int min, char *busid, size_t len)
+{
+    char dname[SPECNAMELEN];
+    char sysctl_name[32];
+    size_t size;
+    int id, type;
+    unsigned int rdev;
+
+    rdev = makedev(maj, min);
+    if (!devname_r(rdev, S_IFCHR, dname, sizeof(dname)))
+      return -EINVAL;
+
+    if (sscanf(dname, "drm/%d\n", &id) != 1)
+        return -EINVAL;
+
+    if (get_sysctl_minor_busid(id, busid, len) == 0)
+        return 0;
+
+    type = drmGetMinorType(maj, min);
+    if (type == DRM_NODE_RENDER)
+        id -= 128;
+    else if (type != DRM_NODE_PRIMARY)
+        return -EINVAL;
+    if (id < 0)
+        return -EINVAL;
+    snprintf(sysctl_name, sizeof(sysctl_name), "hw.dri.%d.busid", id);
+    size = len;
+    if (sysctlbyname(sysctl_name, busid, &size, NULL, 0))
+      return -EINVAL;
+
+    return 0;
+}
+
+/*
+ * Drivers of devices that are not on PCI give the bus ID "platform:<name>":
+ * get the name, if the node's device is one.
+ */
+static int get_sysctl_platform_name(int maj, int min, char *name, size_t len)
+{
+    static const char prefix[] = "platform:";
+    char busid[256];
+
+    if (get_sysctl_busid(maj, min, busid, sizeof(busid)) ||
+        strncmp(busid, prefix, strlen(prefix)) != 0)
+        return -ENOENT;
+    strncpy(name, busid + strlen(prefix), len);
+    name[len - 1] = '\0';
+    return 0;
+}
+#endif
+
 static int drmParseSubsystemType(int maj, int min)
 {
 #ifdef __linux__
@@ -3620,7 +3712,13 @@ static int drmParseSubsystemType(int maj, int min)
             return DRM_BUS_VIRTIO;
      }
     return subsystem_type;
-#elif defined(__OpenBSD__) || defined(__DragonFly__) || defined(__FreeBSD__)
+#elif defined(__FreeBSD__)
+    char name[DRM_PLATFORM_DEVICE_NAME_LEN];
+
+    if (get_sysctl_platform_name(maj, min, name, sizeof(name)) == 0)
+        return DRM_BUS_PLATFORM;
+    return DRM_BUS_PCI;
+#elif defined(__OpenBSD__) || defined(__DragonFly__)
     return DRM_BUS_PCI;
 #else
 #warning "Missing implementation of drmParseSubsystemType"
@@ -3649,44 +3747,11 @@ get_pci_path(int maj, int min, char *pci_path)
 #ifdef __FreeBSD__
 static int get_sysctl_pci_bus_info(int maj, int min, drmPciBusInfoPtr info)
 {
-    char dname[SPECNAMELEN];
-    char sysctl_name[16];
     char sysctl_val[256];
-    size_t sysctl_len;
-    int id, type, nelem;
-    unsigned int rdev, majmin, domain, bus, dev, func;
-
-    rdev = makedev(maj, min);
-    if (!devname_r(rdev, S_IFCHR, dname, sizeof(dname)))
-      return -EINVAL;
-
-    if (sscanf(dname, "drm/%d\n", &id) != 1)
-        return -EINVAL;
-    type = drmGetMinorType(maj, min);
-    if (type == -1)
-        return -EINVAL;
-
-    /* BUG: This above section is iffy, since it mandates that a driver will
-     * create both card and render node.
-     * If it does not, the next DRM device will create card#X and
-     * renderD#(128+X)-1.
-     * This is a possibility in FreeBSD but for now there is no good way for
-     * obtaining the info.
-     */
-    switch (type) {
-    case DRM_NODE_PRIMARY:
-         break;
-    case DRM_NODE_RENDER:
-         id -= 128;
-         break;
-    }
-    if (id < 0)
-        return -EINVAL;
+    int nelem;
+    unsigned int domain, bus, dev, func;
 
-    if (snprintf(sysctl_name, sizeof(sysctl_name), "hw.dri.%d.busid", id) <= 0)
-      return -EINVAL;
-    sysctl_len = sizeof(sysctl_val);
-    if (sysctlbyname(sysctl_name, sysctl_val, &sysctl_len, NULL, 0))
+    if (get_sysctl_busid(maj, min, sysctl_val, sizeof(sysctl_val)))
       return -EINVAL;
 
     #define bus_fmt "pci:%04x:%02x:%02x.%u"
@@ -4300,6 +4365,10 @@ static int drmParseOFBusInfo(int maj, int min, char *fullname)
     free(name);
 
     return 0;
+#elif defined(__FreeBSD__)
+    /* Like Linux without OF data: the name from the bus ID. */
+    return get_sysctl_platform_name(maj, min, fullname,
+                                    DRM_PLATFORM_DEVICE_NAME_LEN);
 #else
 #warning "Missing implementation of drmParseOFBusInfo"
     return -EINVAL;
@@ -4360,6 +4429,25 @@ free:
 
     free(*compatible);
     return err;
+#elif defined(__FreeBSD__)
+    char name[DRM_PLATFORM_DEVICE_NAME_LEN];
+    int err;
+
+    /* Like Linux without OF data: one entry, the device's name. */
+    err = drmParseOFBusInfo(maj, min, name);
+    if (err)
+        return err;
+
+    *compatible = calloc(2, sizeof(char *));
+    if (!*compatible)
+        return -ENOMEM;
+    (*compatible)[0] = strdup(name);
+    if (!(*compatible)[0]) {
+        free(*compatible);
+        return -ENOMEM;
+    }
+
+    return 0;
 #else
 #warning "Missing implementation of drmParseOFDeviceInfo"
     return -EINVAL;
