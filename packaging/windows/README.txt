Ghost Portals for Windows
=========================

The easy way is GhostPortalsSetup.exe from the releases page, which installs
this folder for you and keeps it up to date.

To use it as is: open ghostportals.exe (the app), or run from a terminal in
this folder:

  ghostportals-windows.exe selftest
  ghostportals-windows.exe run --connect <linux-host>:47800

Keep the folder together: the program loads GStreamer from the DLLs beside it
and the plugins in gstreamer\plugins.

Ghost Portals is free software under the GNU GPL, version 3 or later (see
LICENSE). This folder also contains GStreamer (https://gstreamer.freedesktop.org/)
and its dependencies, built by the GStreamer project and distributed under
their own licenses, mostly the LGPL; x264 is GPL. Their source code is available
from https://gstreamer.freedesktop.org/src/ and the projects' own sites.

Source code for Ghost Portals: https://github.com/Adam-AtlasSoftware/GhostPortals
