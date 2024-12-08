## Summary
Simple binary application that pings a list of hosts periodically, and
if more than 50% of those hosts have not returned a successful response
to that ping some configurable number of times in a row, then send a
message to the Fritz!Box to reboot it.

This is necessary for me, as sometimes my internet connection stops working
but the Fritz!Box is still up and running. This is particularly
problematic if I'm away from home, so I've built this script to try and
handle that situation.

Thanks to [nicoh88/cron_fritzbox-reboot](https://github-com.translate.goog/nicoh88/cron_fritzbox-reboot)
for the necessary structure for the reboot request.

## Building
```shell
cargo build --release
```
## Configuration
Copy [`config.sample.toml`](config.sample.toml) to `config.toml`
and edit the options as required. You must specify at least one IP
address, along with the username and password of the Fritz!Box.

## Installing
Recommend using Systemctl. First, copy [`internet-status-checker.sample.service`](internet-status-checker.sample.service)
to `internet-status-checker.service`, and ensure the paths are correctly set, then run:
```shell
sudo ln -s $PWD/internet-status-checker.service /etc/systemd/system/
sudo systemctl enable internet-status-checker
sudo systemctl start internet-status-checker
```