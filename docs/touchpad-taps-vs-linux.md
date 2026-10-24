# Touchpad taps: comparison with the Linux implementation

This is what the Linux reference implementation does, and how DragOS's
implementation lines up with it. The reference is libinput (the gesture and
tap engine) plus the compositor (which only enables and configures it).

## What Linux does

From the libinput documentation (the reference implementation of touchpad
gestures):

* **Tap-to-click is libinput's job, not the compositor's.** libinput detects
  the tap and synthesises the button click. A compositor's only job is to
  *enable* it (`libinput_device_config_tap_set_enabled()`) and to choose the
  settings.

* **The mapping is by finger count, not by tap count:**

  > libinput implements tapping for one, two, and three fingers, where
  > supported by the hardware, and maps those taps into a left, right, and
  > middle button click, respectively.

  So on Linux a *one-finger* tap is a left click, a *two-finger* tap is a
  right click and a *three-finger* tap is a middle click. The number of taps
  in quick succession is not a button selector at all.

* **A tap has to meet several constraints** before it becomes a click:

  > - the touch down and touch up must happen within an implementation-defined
  >   timeout
  > - if a finger moves more than an implementation-defined distance while in
  >   contact, it's not a tap
  > - a tap not meeting required pressure thresholds can be ignored as
  >   accidental touch

* **Tapping is off by default** on most devices, and **on by default** on
  devices where tapping is the only way to trigger a button click:

  > Tapping is enabled by default on devices where tapping is the only method
  > to trigger button clicks. This includes devices without physical buttons
  > such as touch-capable graphics tablets.

* **A touchscreen is a different case.** libinput does *not* interpret
  touchscreen gestures at all:

  > Touchscreen gestures are not interpreted by libinput. Rather, any touch
  > point is passed to the caller and any interpretation of gestures is up to
  > the caller.

  For a touchscreen, the *caller* -- the compositor -- is the one that
  decides what a touch means.

* **Do not reimplement tap-to-click on top of libinput's hold gestures:**

  > Summary: do not use hold gestures to do your own tap-to-click
  > implementation

  libinput warns that the hold-gesture data is not reliable enough for it, and
  that its thresholds depend on whether tap-to-click is enabled.

## What DragOS does, and how it differs

DragOS has two distinct cases, and they are *not* the same thing:

| | Linux | DragOS |
|---|---|---|
| A touchpad (libinput recognises it) | libinput does the tap detection and synthesises the click; the compositor only enables it | same -- `mywima` already calls `config_tap_set_enabled(true)` for a device that has the gesture/touch capability |
| A touchscreen (libinput passes the touches through) | the compositor interprets the touches | `mywima`'s tap detector (`src/touchpad.rs`) counts the taps and maps 1/2/3 to left/right/middle, and the click is synthesised through the seat |

So the implementation follows the Linux split: the tap *count* mapping lives
in the compositor because it is the compositor's job on a touchscreen, and
libinput keeps its own mapping where libinput owns the device.

**Where DragOS deliberately differs from Linux:** the mapping is by *tap
count* (one tap, two taps, three taps) rather than by *finger count*. That is
what was asked for, and it is the mapping a single-point device can actually
offer: the guest's virtual pointing device reports one contact, so a
finger-count mapping would always be "one finger" and could never produce a
right or a middle click. The trade-off is that the first click is held back
until the double-tap window closes, which is the same trade-off every desktop
makes for double clicks.

The tap constraints are the same ones libinput lists: a maximum duration
(250ms) and a maximum movement (12 pixels) while in contact.

## The blocker in the guest

The tap detector is fed by *touch* events, and the guest's pointing device
does not produce any, for two reasons that are both about udev:

1. **libinput classifies a device from its udev properties.** The guest has
   no udev, so libinput sees a device with no properties at all. It cannot
   tell that the virtual tablet is a touch device, so it does not deliver
   touch events for it -- `has_capability(DeviceCapability::Touch)` is false.

2. **The same missing classification is why the button *release* never
   arrives.** The guest's pointing device delivers the button press but not
   the release, so a tap built from button events could never complete. This
   was measured in the guest: the kernel pushes both the press and the
   release (the evdev queue shows `ev_key code=272 val=1` and
   `ev_key code=272 val=0`), and the compositor receives only the press.

Because of the second point, the compositor deliberately does **not** route a
*button* device through the tap detector: swallowing the press would leave
the guest without a left click at all, and the tap could never complete.

## What would make it work

In order of cost:

1. **Give the guest a udev** (or at least the properties libinput reads).
   This is the real fix: libinput would classify the pointing device as a
   touch device, deliver the touch events, and do its own tap-to-click. It
   is a large dependency for what it buys.

2. **Read the evdev node directly in the compositor** for the devices it
   wants to interpret itself. The compositor already has the node open
   through libinput; opening `/dev/input/eventN` a second time and doing its
   own framing is possible but duplicates what libinput does.

3. **Have the kernel report the tip as a touch** (BTN_TOUCH +
   BTN_TOOL_FINGER) and let the compositor interpret the touches. This was
   tried: with the tip reported as a touch, libinput still swallowed the
   events, because the classification is what decides that, not the
   capability bits.

The tap detector itself is written against the touch-event API and is
ready for any of the three.
