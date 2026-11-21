Add `barged`, the machine daemon that reports state and performs sleep, restart
and shutdown with a cancellable countdown, the client and `barge machine
status|sleep|restart|shutdown|cancel` verbs, and a Linux install guide with a
systemd unit and a polkit rule for exactly those three logind actions.
