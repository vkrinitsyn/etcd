# ETCD based QUEUE

## Shortly

Everything from:
- /queue/{q_name}/producer/*

will be moved to: 
- /queue/{q_name}/{idx}/{key}

then copied to:
- /queue/{q_name}/consumer/{client_id}/{idx}/{key}

The pipeline looks like this:

producer -> producer node -> dispatcher node -> consumer node -> client
- all might be on same node


## In detail

### Create a queue

On a first incoming event, the node become a queue leader (dispatcher), if no queue definition set, or existing node not available.

- key: /q/{q_name} OR /queue/{q_name}
- value: queue coordinator node id

The key must be a string starting with /q/ OR /queue/ 
This KV distributed to the whole cluster. So, this will take time to get more than half nodes confirm the new value set.

> [!TODO]
> - Change the leader, if current leader not handling messages recently
> - loaded queue definition from DB,
> - get a command to create a queue


the node as queue dispatcher put rsvr on a queue, to
 - handle /q/{q_name}/p/ create event to dispatch
 - handle /q/{q_name}/c/{client_id}/{idx}/ drops
 - handle /q/{q_name}/c/{client_id}/ creates a new consumer


### Create a consumer
client as etcd consumer creates key local key and start etcd watcher for client assigned
/queue/{q_name}/consumer/{client_id}/
/q/{q_name}/c/{client_id}/
 value is a clients host node, 
 the key will drop, if client wont work for a while
 if client erase or update value, dispatcher will automatically overwrite to current client host
 the copy set to a queue coordinator node
 client can only delete from this path


### Send a messages by producer

producer as etcd client put msg (a value) to node it's connected, 
	then the node copy to a coordinator aka dispatcher node:
/queue/{q_name}/producer/{client_id}/{key-{uuid}}
/q/{q_name}/p/{client_id}/{key}
/q/{q_name}/input/{client_id}/{key}
/q/{q_name}/i/{client_id}/{key}
	0-1 network synch calls, no locks

### Place into the ordered queue 
 
The dispatcher node move to ordered queue:
/queue/{q_name}/{idx}/{key}
operation depends on reliability configuration: more than half nodes or at least few nodes or no set
	variable network calls, might or might not wait to response, no locks

### Distribute to a consumer related node

The dispatcher command to consumer node to copy to notify client:
Client etcd watcher get notifyed of a new event, in a key value:
/queue/{q_name}/consumer/{client_id}/{idx}/{key}:
/q/{q_name}/c/{client_id}/{idx}/{key}:
	no network call if no response on [2] required OR same node
	otherwise no network data call, only command

### Handle the event and acknowledge it

Once client(_id) get etcd watch notification from .../c/{client_id}/{idx}/{key}, 
	1. client load message from KV value, 
	2. processe it
	3. deleted from his queue, 
	 - The client_id can only delete from path /queue/{q_name}/consumer/{client_id}/
	Then dispatcher remove from indexed queue: /queue/{q_name}/{idx}/{key}

	deferred variable network calls, no locks

