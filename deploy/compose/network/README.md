# Network

Leave this directory empty to use Anytype's own network.

For a self-hosted [any-sync](https://github.com/anyproto/any-sync) network, put
its client configuration here as `client.yml` (the node addresses and peer ids
of the deployment; any-sync-dockercompose writes it to `etc/client.yml`). The
bot is then created in that network, or logs in to it with
`ANYTYPE_ACCOUNT_KEY`. An account belongs to one network for good: to switch,
remove the `anytype-data` and `anytype-config` volumes and start again.
