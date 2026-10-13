#!/bin/sh
# The CI's OpenBao identity: policy and AppRole `zeughaus-ci`, and a fresh
# secret_id sealed with systemd-creds for the user zeughaus-ci. Runs on
# sadala, next to OpenBao, as root (deploy/install-sadala.sh runs it with
# sudo). Rerunning rotates: every older secret_id of the role is destroyed
# once the new one is sealed. Prints the role_id that [secrets] in ci.toml
# needs. No secret is printed or written in the clear.
set -eu
ci_user=zeughaus-ci
cred=/var/lib/zeughaus-ci/state/bao-secret-id.cred

export BAO_ADDR=http://10.8.0.1:8200
BAO_TOKEN=$(sed -n 's/^Initial Root Token: //p' /root/openbao-init.txt)
export BAO_TOKEN
role=auth/approle/role/zeughaus-ci

bao policy write zeughaus-ci - >/dev/null <<'POLICY'
path "secret/data/zeughaus/ci" {
  capabilities = ["read"]
}
POLICY
# The CI reaches OpenBao over WireGuard or loopback from this host only.
bao write "$role" token_policies=zeughaus-ci token_ttl=15m token_max_ttl=1h \
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
printf 'role_id = "%s"\n' "$role_id"
