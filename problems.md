# etcd - known problems

Open problems of the embedded etcd and its peer-to-peer queue, most severe first, on the
scale severe / warning / low. Each says what, when, what it does, the workaround, and
what a fix needs. The queue's design: queue.md, queue-p2p-route.md.

None open.

## Not etcd's to fix

**A Spare peer is promoted only by `member_promote`.** A peer that failed a write
is held Spare and gets no writes until it is promoted; promoting it is the
cluster membership's decision - the embedder's - not etcd's. Promoted, it gets
the registry whole (agent.md, "Queues"), so a late promotion misses nothing.
