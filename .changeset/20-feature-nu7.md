---
default: minor
---

# `zainod`: NU7 on Mainnet

NU7 activates on Mainnet at height 3,543,405 (branch id `0x77190ad9`). zainod's compiled schedule
carries it (`zcash_protocol` 0.11.0), so from that height:

- Headers check against ZIP 218's 25 s target spacing and 102-block averaging window.
- `GetLightdInfo` names NU7 as the next upgrade below the height, then its branch id from it.
- The end-of-service alarm fires within a week of target time of a validator's halt, at each
  height's spacing (75 s, then 25 s).

Trusted validators and p2p peers must run Zebra 7.0.0 or later: zainod's peer layer speaks network
protocol version 170190, and a Zebra 6 validator leaves the consensus chain at the activation height.
