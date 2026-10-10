#!/bin/sh
# The CI runner's OpenBao identity: policy and AppRole `zeughaus-ci` on
# sadala, and a fresh secret_id sealed with systemd-creds to this machine
# for the user zeughaus-ci. Rerunning rotates: every older secret_id of the
# role is destroyed once the new one is sealed. Prints the role_id that
# [secrets] in ci.toml needs. No secret is printed or written in the clear.
set -eu
ci_user=zeughaus-ci
cred=/var/lib/zeughaus-ci/state/bao-secret-id.cred
server() {
    ssh -o BatchMode=yes sadala "sudo -n sh -s -- $*" <<'EOF'
set -eu
export BAO_ADDR=http://10.8.0.1:8200
BAO_TOKEN=$(sed -n 's/^Initial Root Token: //p' /root/openbao-init.txt)
export BAO_TOKEN
role=auth/approle/role/zeughaus-ci
case "$1" in
setup)
    bao policy write zeughaus-ci - >/dev/null <<'POLICY'
path "secret/data/zeughaus/ci" {
  capabilities = ["read"]
}
POLICY
    bao write "$role" token_policies=zeughaus-ci token_ttl=15m token_max_ttl=1h \
        secret_id_ttl=0 secret_id_num_uses=0 \
        secret_id_bound_cidrs=10.8.0.10/32 token_bound_cidrs=10.8.0.10/32 >/dev/null
    bao read -field=role_id "$role/role-id"
    ;;
secret-id)
    bao write -f -format=json "$role/secret-id" | jq -r '.data.secret_id + " " + .data.secret_id_accessor'
    ;;
prune)
    for a in $(bao list -format=json "$role/secret-id" | jq -r '.[]'); do
        [ "$a" = "$2" ] || bao write "$role/secret-id-accessor/destroy" secret_id_accessor="$a" >/dev/null
    done
    ;;
esac
EOF
}
role_id=$(server setup)
pair=$(server secret-id)
printf '%s' "${pair%% *}" | sudo -u "$ci_user" systemd-creds encrypt --user --uid="$ci_user" --name=bao-secret-id - "$cred"
sudo -u "$ci_user" chmod 600 "$cred"
server prune "${pair#* }"
unset pair
printf 'role_id = "%s"\n' "$role_id"
