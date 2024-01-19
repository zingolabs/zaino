A(n eventual) replacement for lightwalletd, written in Rust.

Currently connects to a lightwalletd, and acts as a man-in-the-middle proxy that does nothing. 
Each RPC we wish to support will be added individually, by connecting to zcashd/zebrad and doing any nessisary processing.
Eventually, we'll no longer have any calls that need to use the lightwalletd, and it can be removed from the network stack entirely.

A note to developers/consumers/contributers: The end goal is not an exact one-to-one port of all existing lwd functionaliy.
We seek to have at least the minimal functionality nessisary for zingo to connect to zingoproxy instead of a lightwalletd, 
and continue to implement any useful caching/preprocessing we can add...but full backwards compatibilty with all preexisting lightwalletd RPCs is not likely.
