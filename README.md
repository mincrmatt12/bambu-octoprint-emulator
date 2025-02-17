# boe

Monitors a Bambu Labs printer (tested against the P1) over MQTT/FTP and exposes a subset of the Octoprint API to query its state.

Intended for use with [MSign](https://g.mm12.xyz/MSign).

## Features

- `GET /api/printer`
  - returns temperature (without history) and printer state
- `GET /api/job`
  - returns file (based on file names from the SD card) with a name
    that resolves to a valid download link (but we don't implement `/api/files` right now)
  - returns progress based on:
    - the percent complete gauge (to fill in `progress.completion`)
    - the estimated time remaining (to fill in `progress.printTimeLeft`)
    - the current gcode line number (to fill in `progress.filepos`)
- `GET /downloads/files/local/<name from /api/job>`
  - retrieves the current GCODE, cached from FTP


