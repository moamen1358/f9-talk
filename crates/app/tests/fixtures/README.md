# Test clips

16 kHz mono 16-bit PCM WAV, all synthetic: made with the Piper TTS voice
`en_GB-alan-medium` (no human recordings).

| File | Words |
|---|---|
| `dictation-en.wav` | Can you push the fix to GitHub and tell Codex to open a pull request? The Terraform report goes to the DevOps team on Thursday, and Claude should switch the Kubernetes cluster from Deepgram to AssemblyAI. |
| `short-en.wav` | Yes, ship it. |
| `pause-en.wav` | The tokenizing model [1.6 s pause] works correctly, so ship it to GitHub. |
| `env-en.wav` | Put the key in the dot env file, and never use an em dash in the Terraform report. |

`tests/e2e_dictation.rs` streams them through `f9-talk simulate`.
