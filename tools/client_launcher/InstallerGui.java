package net.megu.launcher;

import java.awt.BorderLayout;
import java.awt.FlowLayout;
import java.nio.file.Path;
import javax.swing.BoxLayout;
import javax.swing.JButton;
import javax.swing.JCheckBox;
import javax.swing.JComboBox;
import javax.swing.JDialog;
import javax.swing.JFileChooser;
import javax.swing.JLabel;
import javax.swing.JPanel;
import javax.swing.JScrollPane;
import javax.swing.JTabbedPane;
import javax.swing.JTextArea;
import javax.swing.JTextField;
import javax.swing.SwingUtilities;
import javax.swing.WindowConstants;

/** Minimal double-click installer dialog, opened only when a display exists. */
final class InstallerGui {
    private static final String[] MC_VERSIONS = {"1.21.1", "26.3"};

    private InstallerGui() {}

    /** Opens the modal installer dialog and returns the process exit code. */
    static int show() {
        JDialog dialog = new JDialog((java.awt.Frame) null, "megu Installer", true);
        JTabbedPane tabs = new JTabbedPane();
        JTextArea status = new JTextArea(5, 48);
        status.setEditable(false);
        status.setLineWrap(true);
        status.setWrapStyleWord(true);

        JComboBox<String> version = new JComboBox<>(MC_VERSIONS);
        JTextField clientDir = new JTextField(Main.detectMinecraftDir().toString(), 30);
        JCheckBox profile = new JCheckBox("Create launcher profile", true);
        JButton installClient = new JButton("Install client");
        JButton uninstall = new JButton("Uninstall");
        installClient.addActionListener(e -> run(status, () -> Main.installClient(
                Path.of(clientDir.getText().trim()), (String) version.getSelectedItem(),
                profile.isSelected(), null, null)));
        uninstall.addActionListener(e -> run(status, () -> Main.uninstallClient(
                Path.of(clientDir.getText().trim()), (String) version.getSelectedItem(),
                profile.isSelected())));

        JPanel client = new JPanel();
        client.setLayout(new BoxLayout(client, BoxLayout.Y_AXIS));
        client.add(row("Minecraft version: ", version));
        client.add(dirRow(clientDir));
        client.add(profile);
        client.add(buttons(installClient, uninstall));
        tabs.addTab("Client", client);

        JTextField serverDir = new JTextField(
                Main.detectMinecraftDir().resolveSibling("megu-server").toString(), 30);
        JButton installServer = new JButton("Install server");
        installServer.addActionListener(e -> run(status, () -> Main.installServer(
                Path.of(serverDir.getText().trim()), null)));
        JPanel server = new JPanel();
        server.setLayout(new BoxLayout(server, BoxLayout.Y_AXIS));
        server.add(dirRow(serverDir));
        server.add(buttons(installServer));
        tabs.addTab("Server", server);

        dialog.setLayout(new BorderLayout(6, 6));
        dialog.add(tabs, BorderLayout.CENTER);
        dialog.add(new JScrollPane(status), BorderLayout.SOUTH);
        dialog.setDefaultCloseOperation(WindowConstants.DISPOSE_ON_CLOSE);
        dialog.pack();
        dialog.setLocationRelativeTo(null);
        dialog.setVisible(true);
        return 0;
    }

    private interface Task {
        String run() throws Exception;
    }

    private static void run(JTextArea status, Task task) {
        status.setText("Working...");
        Thread worker = new Thread(() -> {
            String message;
            try {
                message = task.run();
            } catch (Exception error) {
                message = "Failed: " + error.getMessage();
            }
            String result = message;
            SwingUtilities.invokeLater(() -> status.setText(result));
        }, "megu-installer");
        worker.setDaemon(true);
        worker.start();
    }

    private static JPanel row(String label, java.awt.Component field) {
        JPanel row = new JPanel(new BorderLayout(4, 0));
        row.add(new JLabel(label), BorderLayout.WEST);
        row.add(field, BorderLayout.CENTER);
        return row;
    }

    private static JPanel dirRow(JTextField field) {
        JPanel row = row("Install directory: ", field);
        JButton browse = new JButton("Browse...");
        browse.addActionListener(e -> {
            JFileChooser chooser = new JFileChooser(field.getText());
            chooser.setFileSelectionMode(JFileChooser.DIRECTORIES_ONLY);
            if (chooser.showOpenDialog(field) == JFileChooser.APPROVE_OPTION) {
                field.setText(chooser.getSelectedFile().getPath());
            }
        });
        row.add(browse, BorderLayout.EAST);
        return row;
    }

    private static JPanel buttons(JButton... items) {
        JPanel row = new JPanel(new FlowLayout(FlowLayout.LEFT));
        for (JButton item : items) {
            row.add(item);
        }
        return row;
    }
}
