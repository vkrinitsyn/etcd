package e2j;

import jakarta.jms.*;
import org.apache.qpid.jms.JmsConnectionFactory;

/**
 * Reads what the etcds bridge sent, as a JMS application does, and prints one line per
 * message: its JMS type, body, id, delivery mode, group and etcd properties.
 * Stops after the count, or when nothing arrives for the idle seconds.
 *
 *   Listen <amqp url> <user> <password> <queue> <count> <idle seconds>
 */
public class Listen {
    public static void main(String[] a) throws Exception {
        ConnectionFactory f = new JmsConnectionFactory(a[1], a[2], a[0]);
        int count = Integer.parseInt(a[4]);
        long idle = Long.parseLong(a[5]) * 1000;
        try (Connection c = f.createConnection()) {
            c.start();
            Session s = c.createSession(false, Session.CLIENT_ACKNOWLEDGE);
            MessageConsumer mc = s.createConsumer(s.createQueue(a[3]));
            for (int i = 0; i < count; i++) {
                Message m = mc.receive(idle);
                if (m == null) { System.out.println("IDLE after " + i); break; }
                String body;
                String type;
                if (m instanceof TextMessage t) { type = "TextMessage"; body = t.getText(); }
                else if (m instanceof BytesMessage b) {
                    type = "BytesMessage";
                    byte[] buf = new byte[(int) b.getBodyLength()];
                    b.readBytes(buf);
                    StringBuilder sb = new StringBuilder();
                    for (byte x : buf) sb.append(String.format("%02x", x));
                    body = sb.toString();
                } else { type = m.getClass().getSimpleName(); body = "?"; }
                System.out.printf("MSG type=%s body=%s id=%s mode=%s group=%s ts=%d etcd_queue=%s etcd_key=%s etcd_index=%s etcd_delivery_key=%s%n",
                    type, body, m.getJMSMessageID(),
                    m.getJMSDeliveryMode() == DeliveryMode.PERSISTENT ? "PERSISTENT" : "NON_PERSISTENT",
                    m.getStringProperty("JMSXGroupID"), m.getJMSTimestamp(),
                    m.getStringProperty("etcd_queue"), m.getStringProperty("etcd_key"),
                    m.getObjectProperty("etcd_index"), m.getStringProperty("etcd_delivery_key"));
                StringBuilder ce = new StringBuilder();
                for (var names = m.getPropertyNames(); names.hasMoreElements(); ) {
                    String n = (String) names.nextElement();
                    if (n.startsWith("cloudEvents_")) ce.append(' ').append(n).append('=').append(m.getStringProperty(n));
                }
                if (ce.length() > 0) System.out.println("  CE" + ce);
                m.acknowledge();
            }
        }
    }
}
