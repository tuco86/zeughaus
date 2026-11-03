#!/bin/sh
# The CI's OpenBao identity: policy and AppRole `zeughaus`, the token role
# `zeughaus-job` its job tokens are minted through, the policies of those job
# tokens (`zeughaus-job-<repo>`, one per repository of ci.toml with a grant),
# and a fresh secret_id sealed with systemd-creds for the Unix user
# zeughaus-ci. Runs on sadala, next to OpenBao, as root (deploy/setup-sadala.sh
# runs it with sudo), after the CI's binary and ci.toml are in place: the job
# policies come from `zeughaus-ci job-policies`. Rerunning rotates: every older
# secret_id of the role is destroyed once the new one is sealed. Prints the
# role_id that [secrets] in ci.toml needs. No secret is printed or written in
# the clear.
set -eu
ci_user=zeughaus-ci
state=/var/lib/zeughaus-ci/state
cred=$state/bao-secret-id.cred
role=auth/approle/role/zeughaus

export BAO_ADDR=http://10.8.0.1:8200
BAO_TOKEN=$(sed -n 's/^Initial Root Token: //p' /root/openbao-init.txt)
export BAO_TOKEN

# The CI reads and patches its own document (webhook secrets onboarding
# generates, the Claude sign-in it keeps). It reaches no document of
# hannes/. A job's secrets it never reads: it mints a job token through the
# token role and hands it on wrapped, then revokes it by its accessor.
bao policy write zeughaus - >/dev/null <<'POLICY'
path "secret/data/zeughaus/zeughaus" {
  capabilities = ["read", "patch"]
}
path "auth/token/create/zeughaus-job" {
  capabilities = ["update"]
}
path "auth/token/revoke-accessor" {
  capabilities = ["update"]
}
POLICY

# Job tokens: orphans, because the CI's own token lives at most an hour and a
# child dies with its parent; 10 minutes at most, not renewable, usable from
# the WireGuard network only. The glob lets the CI mint policies it does not
# hold itself.
bao write auth/token/roles/zeughaus-job \
    allowed_policies_glob='zeughaus-job-*' orphan=true renewable=false \
    token_explicit_max_ttl=10m token_type=service \
    token_bound_cidrs=10.8.0.0/24 >/dev/null

# One policy per repository with a grant: its project document and the
# global one, read only. A first install runs this before the CI's binary is
# there; the policies follow when the script runs again.
ci_bin=/var/lib/zeughaus-ci/bin/zeughaus-ci
if [ -x "$ci_bin" ]; then
    policies=$(sudo -u "$ci_user" "$ci_bin" --state-dir "$state" job-policies)
    names=$(printf '%s' "$policies" | jq -r 'keys[]')
    for name in $names; do
        printf '%s' "$policies" | jq -r --arg n "$name" '.[$n]' | bao policy write "$name" - >/dev/null
    done
    for old in $(bao policy list -format=json | jq -r '.[] | select(startswith("zeughaus-job-"))'); do
        printf '%s\n' "$names" | grep -qx "$old" || bao policy delete "$old" >/dev/null
    done
else
    echo "no $ci_bin yet: job policies not written; run this again once it is installed" >&2
fi

# The CI reaches OpenBao over WireGuard or loopback from this host only.
bao write "$role" token_policies=zeughaus token_ttl=15m token_max_ttl=1h \
    secret_id_ttl=0 secret_id_num_uses=0 \
    secret_id_bound_cidrs=10.8.0.1/32,127.0.0.1/32 \
    token_bound_cidrs=10.8.0.1/32,127.0.0.1/32 >/dev/null
role_id=$(bao read -field=role_id "$role/role-id")

pair=$(bao write -f -format=json "$role/secret-id" | jq -r '.data.secret_id + " " + .data.secret_id_accessor')
printf '%s' "${pair%% *}" |
    sudo -u "$ci_user" systemd-creds encrypt --user --uid="$ci_user" --name=bao-secret-id - "$cred"
sudo -u "$ci_user" chmod 600 "$cred"
keep=${pair#* }
unset pair
for a in $(bao list -format=json "$role/secret-id" | jq -r '.[]'); do
    [ "$a" = "$keep" ] || bao write "$role/secret-id-accessor/destroy" secret_id_accessor="$a" >/dev/null
done

# The identity this one replaces.
bao delete auth/approle/role/zeughaus-ci >/dev/null 2>&1 || true
bao policy delete zeughaus-ci >/dev/null 2>&1 || true

printf 'role_id = "%s"\n' "$role_id"
echo "set it as [secrets] role_id in $state/ci.toml, then restart zeughaus-ci"
