"""Unflash: detect and repair photosensitive-hazard flashing in videos.

Pipeline: scan whole video -> problem sections -> proxy + thumbnails ->
suggested / manual frame removals and extensions -> simulate safety check ->
preview render -> full-resolution section render -> final assembly.
"""

__version__ = "0.1.0"
