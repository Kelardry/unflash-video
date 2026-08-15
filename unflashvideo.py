import tkinter as tk
from tkinter import ttk, filedialog, messagebox
import ffmpeg
import json
import os
from PIL import Image, ImageTk
import numpy as np
import subprocess
import tempfile
from dataclasses import dataclass
from typing import Dict, Optional, Tuple, List
import time
import re
import cv2
import numpy as np
from collections import deque
import cv2
import numpy as np
from collections import deque
import subprocess
import os

@dataclass
class FrameEdit:
    """Store edit information for a single frame"""
    frame_number: int
    is_deleted: bool = False
    is_extended: bool = False

def parse_timestamp(timestamp: str) -> float:
    """Parse timestamp in format HH:MM:SS.xxx or shorter versions"""
    if not timestamp or timestamp.isspace():
        return 0.0
    try:
        return float(timestamp)
    except ValueError:
        parts = timestamp.split(':')
        if len(parts) > 3:
            raise ValueError("Invalid timestamp format")
        seconds = 0.0
        multiplier = 1
        for part in reversed(parts):
            try:
                seconds += float(part) * multiplier
                multiplier *= 60
            except ValueError:
                raise ValueError("Invalid timestamp format")
        return seconds


class VideoEditor:
    def __init__(self, root):
      self.root = root
      self.root.title("Flash-Safe Video Editor")
      
      # Video state variables
      self.video_path = None
      self.frame_edits: Dict[int, FrameEdit] = {}
      self.current_start_frame = 0
      self.fps = 0
      self.duration = 0
      self.width = 0
      self.height = 0
      
      # Frame cache for faster loading
      self.frame_cache = {}
      self.max_cache_size = 1000  # Maximum number of frames to cache
      
      # Selection state variables
      self.selected_frames = set()  # Set of frame numbers that are currently selected
      self.shift_selection_start = None  # First frame clicked with shift held
      self.is_shift_selecting = False  # Whether we're in the middle of a shift-selection
      
      # Default number of columns for grid layout
      self.num_columns = 4
      
      self._setup_ui()
    
    def _setup_ui(self):
        """Create the main UI elements"""
        # Top controls
        controls_frame = ttk.Frame(self.root)
        controls_frame.pack(fill='x', padx=5, pady=5)
        
        ttk.Button(controls_frame, text="Open Video", command=self._open_video).pack(side='left', padx=5)
        ttk.Button(controls_frame, text="Create Proxy", command=self._create_proxy).pack(side='left', padx=5)
        ttk.Label(controls_frame, text="Start Time:").pack(side='left', padx=5)
        self.start_time_var = tk.StringVar(value="0.0")
        ttk.Entry(controls_frame, textvariable=self.start_time_var, width=15).pack(side='left')
        
        ttk.Label(controls_frame, text="Duration (s):").pack(side='left', padx=5)
        self.duration_var = tk.StringVar(value="0.1")
        ttk.Entry(controls_frame, textvariable=self.duration_var, width=10).pack(side='left')
        
        ttk.Button(controls_frame, text="Load Frames", command=self._load_frame_range).pack(side='left', padx=5)
        
        # Progress bar for frame loading
        self.progress_var = tk.DoubleVar()
        self.progress_bar = ttk.Progressbar(
            controls_frame,
            variable=self.progress_var,
            maximum=100,
            mode='determinate'
        )
        self.progress_bar.pack(side='left', padx=5, fill='x', expand=True)
        
        # Buttons frame
        buttons_frame = ttk.Frame(self.root)
        buttons_frame.pack(fill='x', padx=5, pady=5)
        
        ttk.Button(buttons_frame, text="Preview", command=self._preview_edit).pack(side='left', padx=5)
        ttk.Button(buttons_frame, text="Save Edits", command=self._save_edits).pack(side='left', padx=5)
        ttk.Button(buttons_frame, text="Load Edits", command=self._load_edits).pack(side='left', padx=5)
        ttk.Button(buttons_frame, text="Clear Current", command=self._clear_current_markings).pack(side='left', padx=5)
        ttk.Button(buttons_frame, text="Export", command=self._export_video).pack(side='left', padx=5)
        
        # Rest of the UI setup remains the same as your original code
        # (Canvas and thumbnails frame setup)
        self.canvas_frame = ttk.Frame(self.root)
        self.canvas_frame.pack(fill='both', expand=True, padx=5, pady=5)
        
        self.canvas = tk.Canvas(self.canvas_frame)
        scrollbar = ttk.Scrollbar(self.canvas_frame, orient='vertical', command=self.canvas.yview)
        
        self.canvas.configure(yscrollcommand=scrollbar.set)
        
        scrollbar.pack(side='right', fill='y')
        self.canvas.pack(side='left', fill='both', expand=True)
        
        self.thumbnails_frame = ttk.Frame(self.canvas)
        self.canvas.create_window((0, 0), window=self.thumbnails_frame, anchor='nw')
        
        self.thumbnails_frame.bind('<Configure>', self._configure_scroll_region)
        self.canvas.bind('<Configure>', self._configure_thumbnail_frame)
        self.root.bind('<Escape>', self._clear_selection)
      
    def _create_proxy(self):
      """Create a low-resolution proxy video for faster frame loading using ffmpeg-python"""
      if not self.video_path:
          messagebox.showerror("Error", "Please open a video first")
          return
          
      save_path = filedialog.asksaveasfilename(
          defaultextension=".mp4",
          filetypes=[("MP4 files", "*.mp4")]
      )
      if not save_path:
          return
          
      try:
          # Create 540p proxy with same framerate and audio
          stream = (
              ffmpeg
              .input(self.video_path)
              .output(
                  save_path,
                  vcodec='libx264',
                  crf=28,
                  preset='faster',
                  acodec='copy',  # Copy original audio stream
                  **{
                      'vf': 'scale=-1:540',  # Scale to 540p maintaining aspect ratio
                      'map': '0',  # Map all streams from input
                      'c:a': 'copy',  # Explicitly copy audio codec
                      'movflags': '+faststart'  # Optimize for web playback
                  }
              )
              .overwrite_output()
          )
          
          # Run ffmpeg command
          stream.run()
          
      except Exception as e:
          messagebox.showerror("Error", f"Failed to create proxy video: {str(e)}")
          # If ffmpeg-python method fails, try subprocess method
          try:
              self._create_proxy_subprocess(save_path)
          except Exception as sub_e:
              messagebox.showerror("Error", f"Subprocess method also failed: {str(sub_e)}")
      else:
          messagebox.showinfo("Success", f"Proxy video created successfully: {os.path.basename(save_path)}")
            
    
    def _open_video(self):
        """Open a video file and initialize video properties using ffmpeg"""
        self.video_path = filedialog.askopenfilename(
            filetypes=[("Video files", "*.mp4 *.avi *.mkv")]
        )
        if not self.video_path:
            return
            
        try:
            # Get video information using ffmpeg
            probe = ffmpeg.probe(self.video_path)
            video_info = next(s for s in probe['streams'] if s['codec_type'] == 'video')
            
            # Store video properties
            self.fps = eval(video_info['r_frame_rate'])  # Convert fraction to float
            self.duration = float(probe['format']['duration'])
            self.width = int(video_info['width'])
            self.height = int(video_info['height'])
            
            # Clear existing frame edits
            self.frame_edits.clear()
            messagebox.showinfo("Success", f"Loaded video: {os.path.basename(self.video_path)}")
            
        except Exception as e:
            messagebox.showerror("Error", f"Failed to load video: {str(e)}")
    
    def _extract_frame(self, time_seconds: float) -> Optional[np.ndarray]:
        """Extract a single frame from the video at specified time with caching"""
        # Check cache first
        cache_key = f"{time_seconds:.3f}"
        if cache_key in self.frame_cache:
            return self.frame_cache[cache_key]
            
        try:
            out, _ = (
                ffmpeg
                .input(self.video_path, ss=time_seconds)
                .filter('select', 'gte(n,0)')
                .output('pipe:', vframes=1, format='rawvideo', pix_fmt='rgb24')
                .run(capture_stdout=True, quiet=True)
            )
            
            frame = np.frombuffer(out, np.uint8).reshape([self.height, self.width, 3])
            
            # Cache the frame
            if len(self.frame_cache) >= self.max_cache_size:
                # Remove oldest frame if cache is full
                oldest_key = next(iter(self.frame_cache))
                del self.frame_cache[oldest_key]
            
            self.frame_cache[cache_key] = frame
            return frame
            
        except ffmpeg.Error:
            return None
    
    def _configure_scroll_region(self, event=None):
      """Update the scroll region when the thumbnail frame changes"""
      self.canvas.configure(scrollregion=self.canvas.bbox('all'))
    
    def _configure_thumbnail_frame(self, event=None):
      """Update the thumbnail frame width when the canvas changes"""
      if event:
          # Calculate number of columns based on available width
          THUMB_WIDTH = 160
          PADDING = 10
          total_item_width = THUMB_WIDTH + (2 * PADDING)
          
          # Calculate maximum number of columns that fit
          available_width = event.width - 20  # Account for scrollbar
          self.num_columns = max(1, available_width // total_item_width)
          
          # Reload frames to update layout if frames are already loaded
          if hasattr(self, 'current_start_frame'):
              self._load_frame_range()
    
    def _load_frame_range(self):
      """Load and display frames for the specified time range"""
      if not self.video_path:
          messagebox.showerror("Error", "Please open a video first")
          return
          
      try:
          start_time = parse_timestamp(self.start_time_var.get())
          duration = float(self.duration_var.get())
      except ValueError as e:
          messagebox.showerror("Error", f"Invalid time format: {str(e)}")
          return
      
      # Clear existing thumbnails
      for widget in self.thumbnails_frame.winfo_children():
          widget.destroy()
      
      start_frame = int(start_time * self.fps)
      num_frames = int(duration * self.fps)
      self.current_start_frame = start_frame
      
      # Use dynamic column count from _configure_thumbnail_frame
      THUMB_WIDTH = 160
      THUMB_HEIGHT = 90
      PADDING = 10
      
      columns = getattr(self, 'num_columns', 4)  # Default to 4 if not set
      
      # Create container frame with fixed width
      display_frame = ttk.Frame(self.thumbnails_frame)
      display_frame.pack(side='left', fill='both', expand=True)
      
      # Extract frames with progress updates
      for i in range(num_frames):
          frame_time = start_time + (i / self.fps)
          frame = self._extract_frame(frame_time)
          
          if frame is None:
              break
              
          # Update progress
          self.progress_var.set((i + 1) / num_frames * 100)
          self.root.update()
          
          # Create and place thumbnail
          frame_small = Image.fromarray(frame).resize((THUMB_WIDTH, THUMB_HEIGHT))
          photo = ImageTk.PhotoImage(image=frame_small)
          
          row = i // columns
          col = i % columns
          
          label = tk.Label(
              display_frame,
              image=photo,
              borderwidth=2,
              relief='solid',
              highlightthickness=2,
              highlightbackground='black'
          )
          label.image = photo
          label.frame_number = start_frame + i
          
          label.grid(row=row, column=col, padx=PADDING, pady=PADDING)
          
          frame_number = start_frame + i
          
          # Bind click events
          label.bind('<Button-1>', lambda e, fn=frame_number: self._handle_frame_click(e, fn))
          label.bind('<ButtonRelease-1>', lambda e, fn=frame_number: self._stop_extend_timer(e, fn))
          label.bind('<Button-3>', lambda e, f=frame: self._show_full_frame(e, f))
          
          # Apply existing edits if any
          if frame_number in self.frame_edits:
            self._update_frame_appearance(label, self.frame_edits[frame_number])
      
      # Reset progress bar
      self.progress_var.set(0)
      
      # Update scroll region
      self.thumbnails_frame.update_idletasks()
      self._configure_scroll_region()
    
    def _update_frame_appearance(self, label, edit):
      """Update the visual appearance of a frame based on its edit and selection status"""
      # Get frame number from label
      frame_number = getattr(label, 'frame_number', None)
      
      # Reset to default appearance first
      label.configure(
          borderwidth=2,
          relief='solid',
          bg='SystemButtonFace',
          highlightthickness=3,   
          highlightbackground='black'
      )
      
      # Create or get corner markers
      for marker_name in ['nw_marker', 'ne_marker', 'sw_marker', 'se_marker']:
          marker = getattr(label, marker_name, None)
          if not marker:
              marker = tk.Label(label, width=1, height=1)
              setattr(label, marker_name, marker)
      
      # Set base color based on edit status
      if edit.is_deleted:
          base_color = '#CC0000'  # Dark red
      elif edit.is_extended:
          base_color = '#0066CC'  # Dark blue
      else:
          base_color = 'black'
          
      # Configure border color
      label.configure(highlightbackground=base_color)
      
      # Show/hide corner markers based on selection
      if frame_number in self.selected_frames:
          # Position and show corner markers
          label.ne_marker.place(x=label.winfo_width()-8, y=0)
          label.sw_marker.place(x=0, y=label.winfo_height()-8)
          
          # Make markers visible with selection color
          for marker in [label.ne_marker, label.sw_marker]:
              marker.configure(bg='#00CC00')
      else:
          # Hide corner markers
          for marker in [label.nw_marker, label.ne_marker, label.sw_marker, label.se_marker]:
              marker.place_forget()
    
    def _clear_current_markings(self):
      """Clear deletion and extension markings for currently loaded frames"""
      if not self.video_path:
          messagebox.showerror("Error", "No video loaded")
          return
          
      try:
          start_time = parse_timestamp(self.start_time_var.get())
          duration = float(self.duration_var.get())
          
          start_frame = int(start_time * self.fps)
          end_frame = start_frame + int(duration * self.fps)
          
          # Remove edits only for frames in current view
          frames_to_remove = []
          for frame_num in self.frame_edits.keys():
              if start_frame <= frame_num < end_frame:
                  frames_to_remove.append(frame_num)
          
          for frame_num in frames_to_remove:
              del self.frame_edits[frame_num]
          
          # Refresh display
          self._load_frame_range()
          messagebox.showinfo("Success", "Cleared markings for current frames")
          
      except ValueError as e:
          messagebox.showerror("Error", f"Invalid time format: {str(e)}")
    
    def _start_extend_timer(self, event, frame_number):
      """Start timer for detecting long press for frame extension"""
      # Store the frame number, label, and whether this became a long press
      self._current_frame = frame_number
      self._current_label = event.widget
      self._is_long_press = False  # Track if this becomes a long press
      # Start timer for long press detection
      self._extend_timer = self.root.after(500, self._handle_long_press)
    
    def _stop_extend_timer(self, event, frame_number):
      """Cancel extension timer if released before threshold"""
      # Only process as a click if this wasn't a long press
      if hasattr(self, '_extend_timer'):
          try:
              self.root.after_cancel(self._extend_timer)
          except tk.TclError:
              pass  # Timer already executed
          
          # Only process as a click if this wasn't a long press
          if not getattr(self, '_is_long_press', False):
              self._toggle_delete_frame(event, frame_number)
      
      # Clean up
      if hasattr(self, '_current_frame'):
          del self._current_frame
      if hasattr(self, '_current_label'):
          del self._current_label
      if hasattr(self, '_extend_timer'):
          del self._extend_timer
      if hasattr(self, '_is_long_press'):
          del self._is_long_press
    
    def _handle_long_press(self):
      """Handle long press event by toggling frame extension"""
      if hasattr(self, '_current_frame'):
          self._is_long_press = True  # Mark this as a long press
          self._toggle_extend_frame(self._current_frame)
    
    def _toggle_delete_frame(self, event, frame_number):
      """Toggle frame deletion status"""
      # Create new edit entry if it doesn't exist
      if frame_number not in self.frame_edits:
          self.frame_edits[frame_number] = FrameEdit(frame_number)
      
      # Toggle deletion status
      edit = self.frame_edits[frame_number]
      edit.is_deleted = not edit.is_deleted
      # Ensure extended is False when deleted is True
      if edit.is_deleted:
          edit.is_extended = False
      
      # Update the frame's appearance
      self._update_frame_appearance(event.widget, edit)
    
    def _toggle_extend_frame(self, frame_number):
      """Toggle frame extension status"""
      # Create new edit entry if it doesn't exist
      if frame_number not in self.frame_edits:
          self.frame_edits[frame_number] = FrameEdit(frame_number)
      
      # Toggle extension status
      edit = self.frame_edits[frame_number]
      edit.is_extended = not edit.is_extended
      # Ensure deleted is False when extended is True
      if edit.is_extended:
          edit.is_deleted = False

      # Update the frame's appearance if we have the current label
      if hasattr(self, '_current_label'):
          self._update_frame_appearance(self._current_label, edit)
    
    def _show_full_frame(self, event, frame):
      """Show full-size frame in a new window"""
      preview_window = tk.Toplevel(self.root)
      preview_window.title("Frame Preview")
      
      # Convert frame to PhotoImage
      frame_rgb = cv2.cvtColor(frame, cv2.COLOR_BGR2RGB)
      photo = ImageTk.PhotoImage(Image.fromarray(frame_rgb))
      
      label = ttk.Label(preview_window, image=photo)
      label.image = photo
      label.pack()
      
      # Bind escape key to close window
      preview_window.bind('<Escape>', lambda e: preview_window.destroy())
    
    def _clear_selection(self, event=None):
      """Clear all frame selections"""
      self.selected_frames.clear()
      self.shift_selection_start = None
      self.is_shift_selecting = False
      
      # Update appearance of all visible frames
      for widget in self.thumbnails_frame.winfo_children():
        if isinstance(widget, ttk.Frame):  # Container frame
          for label in widget.winfo_children():
            if isinstance(label, tk.Label) and hasattr(label, 'frame_number'):
              frame_number = label.frame_number
              edit = self.frame_edits.get(frame_number, FrameEdit(frame_number))
              self._update_frame_appearance(label, edit)
    
    def _handle_frame_click(self, event, frame_number):
      """Handle frame click with or without shift modifier"""
      is_shift = event.state & 0x1  # Check if shift is held
      
      if is_shift:
        if not self.shift_selection_start:
          # Start new shift selection
          self.shift_selection_start = frame_number
          self.is_shift_selecting = True
          self.selected_frames = {frame_number}
          self._update_visible_frames()
        else:
          # Complete shift selection
          self._complete_rectangular_selection(frame_number)
          self.is_shift_selecting = False
      else:
        # Regular click - if frame is part of selection, toggle deletion for all selected frames
        if frame_number in self.selected_frames:
          self._toggle_delete_selected_frames()
        else:
          # Clear selection and process as normal click
          self._clear_selection()
          self._start_extend_timer(event, frame_number)
    
    def _complete_rectangular_selection(self, end_frame):
      """Complete rectangular selection between start and end frames"""
      if not self.shift_selection_start:
          return
          
      # Calculate grid positions
      start_rel = self.shift_selection_start - self.current_start_frame
      end_rel = end_frame - self.current_start_frame
      
      start_row = start_rel // self.num_columns
      start_col = start_rel % self.num_columns
      end_row = end_rel // self.num_columns
      end_col = end_rel % self.num_columns
      
      # Ensure proper order
      min_row, max_row = min(start_row, end_row), max(start_row, end_row)
      min_col, max_col = min(start_col, end_col), max(start_col, end_col)
      
      # Select all frames in the rectangle
      self.selected_frames.clear()
      for row in range(min_row, max_row + 1):
          for col in range(min_col, max_col + 1):
              frame_num = self.current_start_frame + (row * self.num_columns + col)
              self.selected_frames.add(frame_num)
      
      self._update_visible_frames()
    
    def _toggle_delete_selected_frames(self):
      """Toggle deletion status for all selected frames"""
      if not self.selected_frames:
          return
          
      # Determine new state based on first selected frame
      first_frame = min(self.selected_frames)
      if first_frame in self.frame_edits:
          new_state = not self.frame_edits[first_frame].is_deleted
      else:
          new_state = True
      
      # Apply to all selected frames
      for frame_num in self.selected_frames:
          if frame_num not in self.frame_edits:
              self.frame_edits[frame_num] = FrameEdit(frame_number=frame_num)
          self.frame_edits[frame_num].is_deleted = new_state
          if new_state:  # Ensure extended is False when deleted is True
              self.frame_edits[frame_num].is_extended = False
      
      self._update_visible_frames()
    
    def _update_visible_frames(self):
      """Update the appearance of all visible frames"""
      for widget in self.thumbnails_frame.winfo_children():
          if isinstance(widget, ttk.Frame):  # Container frame
              for label in widget.winfo_children():
                  if isinstance(label, tk.Label) and hasattr(label, 'frame_number'):
                      frame_number = label.frame_number
                      edit = self.frame_edits.get(frame_number, FrameEdit(frame_number))
                      self._update_frame_appearance(label, edit)
    
    def _tidy_edits(self):
      """Sort frame markings and remove markings of frames neither extended nor deleted"""
      self.frame_edits = {k: v for k,v in sorted(self.frame_edits.items()) if (v.is_deleted or v.is_extended)}
    
    def _save_edits(self):
      """Save frame edits to JSON file"""
      if not self.frame_edits:
          messagebox.showwarning("Warning", "No edits to save")
          return
          
      save_path = filedialog.asksaveasfilename(
          defaultextension=".json",
          filetypes=[("JSON files", "*.json")]
      )
      if not save_path:
          return
          
      self._tidy_edits() #sort and clean up edit markings
      # Convert frame edits to serializable format
      edits_data = {
          'video_path': self.video_path,
          'edits': {
              str(k): {'is_deleted': v.is_deleted, 'is_extended': v.is_extended}
              for k, v in self.frame_edits.items()
          }
      }
      
      with open(save_path, 'w') as f:
          json.dump(edits_data, f)
      
      messagebox.showinfo("Success", "Edits saved successfully")
    
    def _load_edits(self):
      """Load frame edits from JSON file"""
      if not self.video_path:
          messagebox.showerror("Error", "Please open a video first")
          return
          
      load_path = filedialog.askopenfilename(
          filetypes=[("JSON files", "*.json")]
      )
      if not load_path:
          return
          
      with open(load_path, 'r') as f:
          edits_data = json.load(f)
      
      # Check if edits are from a different video
      if edits_data['video_path'] != self.video_path:
          # Get maximum frame number in edits
          max_edit_frame = max(int(k) for k in edits_data['edits'].keys())
          
          # Get total frames in current video
          cap = cv2.VideoCapture(self.video_path)
          total_frames = int(cap.get(cv2.CAP_PROP_FRAME_COUNT))
          cap.release()
          
          if max_edit_frame >= total_frames:
              messagebox.showerror(
                  "Error",
                  f"Edit file contains frames beyond current video length.\n\n"
                  f"Edit file is from: {edits_data['video_path']}\n"
                  f"Current video is: {self.video_path}"
              )
              return
              
          # Warn user but allow loading
          proceed = messagebox.askyesno(
              "Warning",
              f"Edit file is from a different video:\n\n"
              f"Edit file: {edits_data['video_path']}\n"
              f"Current video: {self.video_path}\n\n"
              "Do you want to load these edits anyway?"
          )
          
          if not proceed:
              return
      
      # Convert loaded data back to FrameEdit objects
      self.frame_edits = {
          int(k): FrameEdit(
              frame_number=int(k),
              is_deleted=v['is_deleted'],
              is_extended=v['is_extended']
          )
          for k, v in edits_data['edits'].items()
      }
      
      # Refresh current view
      self._load_frame_range()
    
    def _process_video(self, output_path: str, preview: bool = False):
      """Process video applying frame deletions and extensions
      
      Args:
          output_path: Path where the processed video will be saved
          preview: If True, only process the currently visible time range
      """
      
      if not self.video_path:
          raise ValueError("No video loaded")
          
      # Determine time range to process
      if preview:
          start_time = parse_timestamp(self.start_time_var.get())
          duration = float(self.duration_var.get())
          end_time = start_time + duration
      else:
          start_time = 0
          end_time = self.duration
          
      self._tidy_edits() #sort and clean up edit markings
      
      # Calculate frame ranges
      start_frame = int(start_time * self.fps)
      end_frame = int(end_time * self.fps)
      
      # Create temporary files for intermediate results
      temp_dir = tempfile.mkdtemp()
      temp_video = os.path.join(temp_dir, 'temp_video.mp4')
      temp_audio = os.path.join(temp_dir, 'temp_audio.wav')
      
      try:
          # Video processing section remains unchanged
          cap = cv2.VideoCapture(self.video_path)
          total_frames = int(cap.get(cv2.CAP_PROP_FRAME_COUNT))
          
          fourcc = cv2.VideoWriter_fourcc(*'mp4v')
          out = cv2.VideoWriter(
              temp_video,
              fourcc,
              self.fps,
              (self.width, self.height)
          )
          
          last_valid_frame = None
          frame_buffer = deque(maxlen=30)
          current_frame_num = 0
          
          while current_frame_num < total_frames:
              ret, frame = cap.read()
              if not ret:
                  break
                  
              if current_frame_num < start_frame:
                  current_frame_num += 1
                  continue
                  
              if current_frame_num >= end_frame and preview:
                  break
                  
              frame_buffer.append(frame.copy())
              
              if current_frame_num in self.frame_edits:
                  edit = self.frame_edits[current_frame_num]
                  
                  if edit.is_deleted:
                      if last_valid_frame is not None:
                          out.write(last_valid_frame)
                      else:
                          out.write(frame)
                  else:
                      out.write(frame)
                      last_valid_frame = frame.copy()
                      
                      if edit.is_extended:
                          for _ in range(int(self.fps)):
                              out.write(frame)
              else:
                  out.write(frame)
                  last_valid_frame = frame.copy()
              
              current_frame_num += 1
          
          cap.release()
          out.release()
          
          # Modified audio processing
          command_parts = [
              'ffmpeg',
              '-i', self.video_path,  # Original audio
              '-f', 'lavfi', '-i', 'anullsrc=r=44100:cl=stereo'  # Silent audio source
          ]
          
          # Find all frame extensions (we don't need to process deletions for audio)
          extensions = sorted([
              (k, v) for k, v in self.frame_edits.items() 
              if start_frame <= k <= end_frame and v.is_extended
          ], key=lambda x: x[0])
          
          segments = []
          concat_inputs = []
          segment_count = 0
          
          # Process audio in segments, only splitting at extension points
          current_pos = start_time
          
          for frame_num, edit in extensions:
              frame_time = frame_num / self.fps
              
              # Add segment from current position to this extension
              if frame_time > current_pos:
                  segments.append(f'[0:a]atrim={current_pos}:{frame_time}[s{segment_count}];')
                  concat_inputs.append(f'[s{segment_count}]')
                  segment_count += 1
              
              # Add the 1-second silence for the extension
              segments.append(f'[1:a]atrim=0:1[s{segment_count}];')
              concat_inputs.append(f'[s{segment_count}]')
              segment_count += 1
              
              current_pos = frame_time
          
          # Add final segment from last extension to end
          if current_pos < end_time:
              segments.append(f'[0:a]atrim={current_pos}:{end_time}[s{segment_count}];')
              concat_inputs.append(f'[s{segment_count}]')
              segment_count += 1
              
          # If no segments were created (no extensions), just trim the audio
          if not segments:
              segments.append(f'[0:a]atrim={start_time}:{end_time}[s{segment_count}];')
              concat_inputs.append(f'[s{segment_count}]')
              segment_count += 1
          
          # Build the complete filter complex
          filter_complex = ''.join(segments)
          if segments:
              filter_complex += f"{''.join(concat_inputs)}concat=n={segment_count}:v=0:a=1[outa]"
          
          # Generate processed audio
          command_parts.extend([
              '-filter_complex', filter_complex,
              '-map', '[outa]',
              '-acodec', 'pcm_s16le',
              temp_audio
          ])
          
          subprocess.run(command_parts, check=True)
          
          # Combine processed video and audio
          subprocess.run([
              'ffmpeg',
              '-i', temp_video,
              '-i', temp_audio,
              '-c:v', 'libx264',
              '-c:a', 'aac',
              '-strict', 'experimental',
              '-map', '0:v:0',
              '-map', '1:a:0',
              output_path
          ], check=True)
          
      finally:
          # Cleanup temporary files
          for temp_file in [temp_video, temp_audio]:
              if os.path.exists(temp_file):
                  try:
                      os.unlink(temp_file)
                  except:
                      pass
          try:
              os.rmdir(temp_dir)
          except:
              pass

    
    def _preview_edit(self):
        """Show preview of edited video section"""
        if not self.video_path:
            messagebox.showerror("Error", "No video loaded")
            return
        
        try:
            # Create temporary video file for preview
            with tempfile.NamedTemporaryFile(suffix='.mp4', delete=False) as temp_file:
                temp_path = temp_file.name
            
            self._process_video(temp_path, preview=True)
            
            # Open preview with default video player
            if os.name == 'nt':  # Windows
                os.startfile(temp_path)
            else:  # Unix-like
                subprocess.run(['xdg-open', temp_path])
        
        except Exception as e:
            messagebox.showerror("Error", f"Preview failed: {str(e)}")
        finally:
            # Clean up temporary file after a delay
            def cleanup():
                if os.path.exists(temp_path):
                    try:
                        os.unlink(temp_path)
                    except:
                        pass
            self.root.after(5000, cleanup)  # Clean up after 5 seconds
    
    def _export_video(self):
        """Export the complete edited video"""
        if not self.video_path:
            messagebox.showerror("Error", "No video loaded")
            return
            
        export_path = filedialog.asksaveasfilename(
            defaultextension=".mp4",
            filetypes=[("MP4 files", "*.mp4")]
        )
        if not export_path:
            return
            
        try:
            self._process_video(export_path)
            messagebox.showinfo("Success", "Video exported successfully")
        except Exception as e:
            messagebox.showerror("Error", f"Export failed: {str(e)}")

def main():
    root = tk.Tk()
    root.geometry("800x600")
    app = VideoEditor(root)
    root.mainloop()

if __name__ == "__main__":
    main()