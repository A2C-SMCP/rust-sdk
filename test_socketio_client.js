const { io } = require('socket.io-client');

async function testSocketIO() {
    console.log('Testing Socket.IO connection to Rust server...');
    
    // Connect to the server
    const agent = io('http://localhost:3000', {
        transports: ['websocket'],
        query: {
            namespace: '/smcp'
        }
    });
    
    // Wait for connection
    await new Promise((resolve) => {
        agent.on('connect', () => {
            console.log('Agent connected with ID:', agent.id);
            resolve();
        });
    });
    
    // Listen for notifications
    agent.on('notify:enter_office', (data) => {
        console.log('Agent received notify:enter_office:', data);
    });
    
    agent.on('notify_enter_office', (data) => {
        console.log('Agent received notify_enter_office:', data);
    });
    
    agent.on('message', (data) => {
        console.log('Agent received message:', data);
    });
    
    // Listen for all events
    agent.onAny((eventName, ...args) => {
        console.log('Agent received any event:', eventName, args);
    });
    
    // Join office
    console.log('Agent joining office...');
    agent.emit('server:join_office', {
        role: 'agent',
        name: 'js-agent',
        office_id: 'test-office-js'
    });
    
    // Wait a bit
    await new Promise(resolve => setTimeout(resolve, 500));
    
    // Create computer client
    const computer = io('http://localhost:3000', {
        transports: ['websocket'],
        query: {
            namespace: '/smcp'
        }
    });
    
    await new Promise((resolve) => {
        computer.on('connect', () => {
            console.log('Computer connected with ID:', computer.id);
            resolve();
        });
    });
    
    // Computer joins office
    console.log('Computer joining office...');
    computer.emit('server:join_office', {
        role: 'computer',
        name: 'js-computer',
        office_id: 'test-office-js'
    });
    
    // Wait for notifications
    await new Promise(resolve => setTimeout(resolve, 2000));
    
    // Disconnect
    agent.disconnect();
    computer.disconnect();
    console.log('Test completed');
}

// Run the test
testSocketIO().catch(console.error);
