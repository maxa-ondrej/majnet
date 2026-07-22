# Secret rotation

Secrets are **inline** in the app manifest as a `secrets:` map — one
`majnet:<base64(age ciphertext)>` line per key (ADR 0024). SOPS is fully retired.
Only the reconciler decrypts (at deploy, into tmpfs); everything else is write-only.

## An app secret (routine)
1. **Configuration** sheet of the app in the dashboard (VPN; `base.yaml` +
   `production.yaml` are admin-gated). Pick the file tab (`base.yaml` for shared
   secrets, or a class overlay), edit the **Secrets** section, then **Save & commit**
   (one button saves the manifest + secrets). The bot `age`-encrypts each value to the
   file's recipient(s) and writes the inline `secrets:` map.
2. Commit to `main` → render PR → (auto-)merge → reconciler blue-greens the app with
   the new tmpfs files. Rotation is just a deploy.

## Encode a secret locally (no dashboard)
Anyone can encode a value on their own machine — plaintext never leaves it:
```sh
R=$(curl -s https://<bot-public>/api/secrets/recipients | jq -r .production)  # or .stable
printf %s "$VALUE" | age -r "$R" | base64 -w0 | sed 's/^/majnet:/'
```
Paste the `majnet:…` line into the manifest `secrets:` map. `production` covers
`base.yaml` + `production.yaml`; `stable` covers the non-prod classes. Encode-only —
the public recipient can't decrypt.

## A platform class key (`age-stable` / `age-production`)
The reconciler is the sole recipient, so rotation = re-encrypt every inline value to
the new key (there are no per-admin recipients to update).
1. `age-keygen -o age-<class>.key.new` on the main node; note its recipient (`age-keygen -y`).
2. Keep the **old** key readable and add the **new** one: decrypt-old/encrypt-new. The
   simplest path is to re-run the migrate/re-encrypt sweep (`POST …/migrate` per app, or
   the reconciler `reencrypt` with the new recipient) so every inline value is rewritten;
   commit across projects (the registry in `projects.yaml` enumerates them).
3. Swap the file in `MAJNET_AGE_KEY_DIR` (+ update `MAJNET_AGE_<CLASS>_RECIPIENT` in
   `bot.env`), recreate the reconciler + bot, verify a converge succeeds.
4. Once every value is re-encrypted to the new key, retire the old key file.

## The DB master key
Don't, unless compromised — every derived password changes. If you must: replace `db-master.key`, then for each app with a `database:` the reconciler re-provisions the *user* password on next converge (ALTER ROLE/USER runs every cycle), and the new config hash redeploys apps with fresh `DATABASE_URL`s. Expect one blue-green wave across the fleet.
