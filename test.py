#!/usr/bin/python3
import etcd3 # require https://github.com/lupko/etcd3-client.git

def run():
    ETCD = etcd3.client()  # host="localhost", port=2379)
    key = 'nodes'
    x = ETCD.put(key, '111')
    print(f"prev: {x}")
    val, meta = ETCD.get(key)
    v = val.decode('UTF-8') + " /from etcd"
    print(f"got: {key}={v}")

# this is a function code start>
#     cur = DB.cursor()
#     cur.execute("SELECT input FROM test_source where id = %s", ([ID]))
#     input = cur.fetchall()
#     if len(input) > 0:
#         cur.execute("insert into test_sink (data) values (%s)", ( input[0] ))
#         DB.commit()

if __name__ == '__main__':
    run()
