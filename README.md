# WezTerm for PicoCalc

This is an implementation of an ssh client and terminal emulator that runs on a
[Raspberry Pi Pico 2
W](https://www.raspberrypi.com/products/raspberry-pi-pico-2/) installed in a
[PicoCalc](https://www.clockworkpi.com/picocalc).

This project is a bit of a toy, but it's a fun one to hack on!

> [!NOTE]
> This will only run on an rp2350 that is pin-compatible with the
> Pico 2W. It requires wifi.

## Status

 * [x] Connect to wifi
 * [x] basic password based ssh auth
 * Terminal Emulation
    * many escape sequences currently not implemented
    * keyboard encoding not yet fully implemented

## Using it

When it first boots it will need to have wifi credentials configured.
Do this using the `config` command, which stores key/value info in
flash, which you should format on first use only:

```console
$ config format
$ config set wifi_ssid YourSSID
$ config set wifi_pw YourPW
$ reboot
```

> [!CAUTION]
> Please note that the config storage is clear-text data held
> in a region of the flash memory on the device. If someone
> has your device, it is possible to extract any credentials
> from it simply by booting it up and running `config list`.

When it reboots, it will attempt to connect, DHCP an IP address
and sync the time from an NTP server.

At that point you can ssh somewhere:

```console
$ ssh hostname
```

You will be prompted for the username and password for the remote
host.

If you don't like typing those things in, you can save them
in the config:

```console
$ config set ssh_user YourUser
$ config set ssh_pw AndPassWord
```

> [!CAUTION]
> Please note that the config storage is clear-text data held
> in a region of the flash memory on the device. If someone
> has your device, it is possible to extract any credentials
> from it simply by booting it up and running `config list`.

## Available Commands

### bat

Show battery charging status and remaining capacity as a percentage.

### bl

Show or manipulate the keyboard or lcd backlight

* `bl kbd PCT` - sets the keyboard backlight level to `PCT` percentage.
   Note that this functionality requires an updated version of the
   picocalc keyboard MCU firmware to be installed on your device,
   as many shipped without this functionality.
   [PR](https://github.com/clockworkpi/PicoCalc/pull/21)
   [How to flash the keyboard MCU](https://github.com/clockworkpi/PicoCalc/blob/master/wiki/Setting-Up-Arduino-Development-for-PicoCalc-keyboard.md)
* `bl lcd PCT` - sets the lcd backlight level to `PCT` percentage.

### bootsel

Reboot into bootsel mode, to facilitate flashing a new firmware image

### cls

Clears the screen

### config

Operates on the config section of flash storage. This is 8KiB in size.

 * `config format` - prepares the flash storage region for first use
 * `config list` - shows the contents of the config storage
 * `config get KEY` - shows the value of `KEY`
 * `config rm KEY` - marks `KEY` as removed
 * `config set KEY VALUE` - assigns `KEY=VALUE`

> [!CAUTION]
> Please note that the config storage is clear-text data held
> in a region of the flash memory on the device. If someone
> has your device, it is possible to extract any credentials
> from it simply by booting it up and running `config list`.

### free

Shows memory usage information

### ls

Shows contents of a FAT SD card.  This is currently very basic and doesn't
support LFN.

### reboot

Reboot the device

### ssh

A very simple ssh client

* `ssh host` - connect to host and start a shell
* `ssh host command` - connect to host and run a command

### time

Show the time

## Building it

You need `flip-link` to re-arrange the memory layout:

```console
$ cargo install flip-link
```

if for some reason this doesn't work out, comment out the `linker` line from
`.cargo/config.toml`, but note that the estimation of available RAM printed
on boot will be incorrect.

