# FLEURS fixture attribution

12 original human speech recordings and their reference transcriptions are selected from **google/fleurs**, revision `70bb2e84b976b7e960aa89f1c648e09c59f894dd`, configurations `cmn_hans_cn` and `en_us`, split `test`.

Source: https://huggingface.co/datasets/google/fleurs

License: **Creative Commons Attribution 4.0 International (CC-BY-4.0)**, as stated in the upstream dataset-card metadata. License terms: https://creativecommons.org/licenses/by/4.0/ and https://creativecommons.org/licenses/by/4.0/legalcode . The pinned upstream dataset card is retained verbatim as `FLEURS-DATASET-CARD.md`.

Please credit:

> Alexis Conneau, Min Ma, Simran Khanuja, Yu Zhang, Vera Axelrod, Siddharth Dalmia, Jason Riesa, Clara Rivera, and Ankur Bapna. **FLEURS: Few-shot Learning Evaluation of Universal Representations of Speech** (2022), arXiv:2205.12446. https://arxiv.org/abs/2205.12446

The official card also credits dataset contributors Patrick von Platen and Alexis Conneau. No endorsement of this application by the dataset authors is implied.

Modifications: each selected WAV is converted locally to mono 16 kHz signed 16-bit PCM with FFmpeg; complete utterances are retained without cropping. `mixed01.wav` and `mixed02.wav` concatenate complete original Chinese and English clips with 250 ms generated digital silence. These derived clips remain under CC-BY-4.0 and are **not natural within-speaker code switching**. Exact archive members, TSV row references, original-file and transformed-file SHA-256 values are in `manifest.json`.

`silence01.wav` consists only of locally generated zero PCM samples, with no recorded voice. The preparation script and this generated silence are dedicated under CC0-1.0: https://creativecommons.org/publicdomain/zero/1.0/ .

The original `sources/` WAVs, converted test WAVs, reference text and hashes provide an audit trail. Selection does not imply that the ASR, rewrite model, protected facts, human quality score, or V10 acceptance has passed.
