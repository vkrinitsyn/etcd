# Approvals of a queue's messages

Feature `approvals` (with `auth`: signon's accounts, and its labac for who may approve).

A queue that has an approval definition is gated. A message put to it is held - no consumer
gets it - until enough different accounts approved it. The votes are collected on a key, the
message's state, and once the count is reached the message goes into the queue for processing,
as any other message. The producer and the consumers of the queue do nothing new.

```
 producer -- put /q/pay/p/k1 --> held on the leader          /approval/pay/k1   (the state)
 ann      -- put /approval/pay/k1 = approve --> 1 of 2       {"need":2,"approved":["ann@.."],..}
 bob      -- put /approval/pay/k1 = approve --> 2 of 2  -->  into /q/pay, to its consumer
```

## The keys

| key | who | what |
|---|---|---|
| `/approval/{queue}` | `root` | the queue's definition: put `{"need": 2}` to gate the queue, delete to open it (refused while a message waits) |
| `/q/{queue}/p/{key}` | a producer, as before | a message: on a gated queue it waits |
| `/approval/{queue}/{key}` | read: anyone with read on the key | the waiting message's state, an ordinary key - get it, watch it |
| `/approval/{queue}/{key}` | put: by level | a vote: the value is the action |
| `/approval/{queue}/{key}` | delete: the producer, `root` | the waiting message withdrawn |

`/q/` and `/queue/` are the two spellings of a queue's keys, as everywhere.

## The definition

```json
{"need": 2, "alias": "queue"}
```

- `need`: how many different accounts approve before the message goes on. 1 or more.
- `alias`: the labac resource kind the queue is a resource of - `queue` unless said. In signon
  the queue is the resource of that kind whose id is the queue's name, and an ACL entry gives a
  group its level there; an entry on resource `*` gives it on every queue.

## Who may what

By signon's approval scale: a level gives its action and every one below it.

| level | action | a vote's value | what it does |
|---|---|---|---|
| 1 | view | | (reading the state is the key's read permission) |
| 2 | comment | `comment <text>` | kept on the state |
| 3 | recommend | `recommend` | kept on the state, counts for nothing |
| 4 | approve | `approve` | counts once an account; the one that completes the count releases the message |
| 5 | veto | `veto` | blocks every approval and drops the ones collected |
| 6 | override | `override` | lifts a veto: the voting starts again |

- An approval is not taken back: there is no vote that disapproves. To approve is not to veto,
  and to veto is not to override.
- The account is the token's, so authentication is on. Its groups are the ones of its labac
  subject - made when it signs in through an identity provider - and its signon roles: an
  account that only ever signed in to etcd is judged by its roles.
- A vote needs no key permission: labac decides it. Putting a message needs write on the queue's
  producer key, as on any queue.

In signon, for the queue `pay` and the role `approvers`:

```rust
labac::put_resource(&pool, &Resource { kind: "queue".into(), id: "pay".into(), ..Default::default() }).await?;
labac::add_entry(&pool, &Entry { kind: "queue".into(), resource: "pay".into(), group: "approvers".into(), level: 4,
    ..Default::default() }).await?;
```

## The state

```json
{"queue": "pay", "need": 2, "by": "pat@example.com", "approved": ["ann@example.com"],
 "recommended": [], "comments": [{"by": "rae@example.com", "text": "matches the order"}],
 "vetoed_by": null, "value": "pay 100"}
```

It is deleted when the message goes into the queue, and when it is withdrawn.

## What to know

- **One node decides**, the leader, as for leases and locks: a put on another node is sent on
  to it with the account's token. A definition and a waiting message with its votes are kept as
  the leader's records - on a majority of the nodes, or in the embedder's store - so a change
  of leader loses none of them.
- **At least once.** The message goes into the queue first and its record after: a leader lost
  between the two leaves the message waiting with its count complete, and its next approval
  releases it again. A consumer's work is safe to run twice, as for any queue.
- **No way around the count**: a transaction that touches these keys or a gated queue, a put on
  another key of a gated queue, and a range delete over `/approval/` are refused - for `root`
  too. The queues of the server's own (`etcds-seq`) are gated by nobody.
- A message key waits once: a second put of the same key is refused until the first went on or
  was withdrawn. There is no limit on how many messages wait.
