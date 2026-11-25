# Custom UI sounds

Heronite's Console Mode sounds can use external sound packs placed beside the executable:

```text
sounds/
└── <style>/
    ├── move.wav
    ├── confirm.wav
    └── ...
```

The supported formats are `.wav`, `.ogg`, `.mp3`, and `.flac`. Use one extension per sound name; if multiple formats are present, Heronite checks WAV, OGG, MP3, then FLAC. Each file must contain one or two channels, use a sample rate up to 192 kHz, and be no longer than five seconds. A style appears in Settings when it contains at least one valid sound.

| File | Played when | Missing-file behavior |
|---|---|---|
| `move.*` | Navigating between items | Built-in sound for the selected style |
| `confirm.*` | Confirming a selection | Built-in sound for the selected style |
| `back.*` | Going back | Built-in sound for the selected style |
| `tab.*` | Changing tabs | Built-in sound for the selected style |
| `launch.*` | A game launch begins | Falls back to `confirm` |
| `notification.*` | A toast or banner appears | Falls back to `confirm` |
| `restore.*` | Heronite's window is restored after a game | Falls back to `confirm` |

Built-in styles provide the navigation and confirmation sounds. A custom `notification` file is loaded from the selected style folder; only when it is absent or invalid does it fall back to `confirm`.

## Dynamic Pitch

When **Dynamic pitch** is enabled, rapid navigation raises the pitch of consecutive `move` sounds through a five-note scale. A pause resets the sequence. It affects only navigation sounds and does not change custom sound files on disk.

## Audio Output Device

Choose **Audio Output Device** in Console Settings. **System Default** follows the operating system's current default output, including when that default changes while Heronite is running. A specific device can also be selected. If it disappears or its audio stream becomes invalid, playback retries and falls back to the system default; a selected device is retried automatically when available again.
