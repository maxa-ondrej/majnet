# Secret rotation

Secrets are **inline** in the app manifest as a `secrets:` map — one
`majnet:<base64(age ciphertext)>` line per key (ADR 0024). Editing goes through the
dashboard; the reconciler is the only decryptor. (Legacy per-class SOPS files are
still read for apps not yet migrated — see "Migrate a legacy app" below.)

## An app secret (routine)
1. **Secrets** section of the app in the dashboard (VPN; production is admin-gated) →
   edit values → Save. The bot encrypts each value with `age` to the class recipient
   and writes the inline `secrets:` map to the class overlay.
2. Commit to `main` → render PR → (auto-)merge → reconciler blue-greens the app with
   the new tmpfs files. Rotation is just a deploy.

## Migrate a legacy app (SOPS → inline)
`POST /api/secrets/{org}/{app}/migrate` (admin) — the reconciler re-encrypts each
class's `secrets.<class>.yaml` to inline ciphertext (plaintext never leaves it) and
the bot commits the inline map + deletes the SOPS file. Idempotent. Review the render
PR(s) to deploy (production).

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
