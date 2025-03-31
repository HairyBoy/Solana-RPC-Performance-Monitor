class WebSocketMonitor extends WebSocket {
    static responseTimesMemoryLength = 150;
    static debugMode = false;

    constructor(url, protocols, name) {
        super(url, protocols)

        this.name = name;
        this.time_of_last_response = Date.now() // technically should not initialize value here TODO
        this.response_times = new Array(WebSocketMonitor.responseTimesMemoryLength).fill(0);

        this.current_slot = -1;
    }

    /*
        Add the newest response time to the stored list and remove the oldest value if max storage reached
    */
    updateResponseTimes(newTime) {
        // Remove the first (oldest) element and add the new value at the end.
        this.response_times.shift();
        this.response_times.push(newTime);
    }

    /*
        Return the average response time from the stored list of the last X responses (where x = responseTimesMemoryLength)
    */
    getAverageResponseTime() {
        // Filter out any zero values.
        const nonZeroTimes = this.response_times.filter(time => time !== 0);
        if (nonZeroTimes.length === 0) {
            return 0;
        }
        // Sum up the non-zero times.
        const sum = nonZeroTimes.reduce((acc, curr) => acc + curr, 0);
        // Return the average.
        return sum / nonZeroTimes.length;
    }

    /*
        Act on the notification of a new processed slot
    */
    processSlotNotification(slot) {

        const cur_time = Date.now();
        const response_time = cur_time - this.getTimeOfLastResponse();
        this.setTimeOfLastResponse(cur_time);
        this.updateResponseTimes(response_time);
        this.setCurrentSlot(slot);

        if (WebSocketMonitor.debugMode == true) {
            console.log('Time between Notifications:', response_time);
            console.log('Average Slot Notification Time:', avgResponseTime);
        }

        this.updateMonitoringPage();
    }

    /*
        Update the monitoring page with latest info about this websocket
    */
    updateMonitoringPage() {

        console.log("updateMonitoringPage" + this.getName())
        // Search for existing page element
        const id = 'webSocket' + this.getName();
        let wsListEl = document.getElementById(id);

        // If the element doesn't exist, create it and append it to a container.
        if (!wsListEl) {
            wsListEl = document.createElement('div');
            wsListEl.id = id;
            // Find the parent container
            const container = document.getElementById('webSocketList') || document.body;
            container.appendChild(wsListEl);
        }

        // Update the page content
        wsListEl.innerHTML = `
            <p><strong>Name:</strong> ${this.getName()} </p>
            <p><strong>Slot:</strong> ${this.getCurrentSlot()} </p>
            <p><strong>Time between slot notifications:</strong> ${this.getLastResponseTime()} ms</p>
            <p><strong>Average time between slot notifications:</strong> ${this.getAverageResponseTime().toFixed(2)} ms</p>
            <p>--------------------------</p>
        `;
    }

    getCurrentSlot() {
        return this.current_slot;
    }
    setCurrentSlot(slot) {
        this.current_slot = slot;
    }

    getName() {
        return this.name;
    }

    getLastResponseTime() {
        return this.response_times[this.response_times.length - 1]
    }

    getTimeOfLastResponse() {
        return this.time_of_last_response
    }
    setTimeOfLastResponse(newTime) {
        this.time_of_last_response = newTime
    }
};


/*
    Function to initialize and connect all websockets
*/
async function initializeWebSockets() {
    const url = '/api/node_websockets';
    try {
      const response = await fetch(url);
      const data = await response.json();
      const endpoints = data.endpoints;
      console.log('Fetched websocket endpoints: ', endpoints);
      endpoints.forEach(endpoint => {
        
        // Create a new websocket monitor
        const wsm = new WebSocketMonitor(endpoint.url, undefined, endpoint.nickname);

        // Attach event listeners
        wsm.addEventListener('open', () => {
            console.log(`Connected to server ${endpoint.nickname}`);
            wsm.send('Hello, server!');

            //subscribe to Slot Notifications
            const requestData = { "jsonrpc": "2.0", "id": 1, "method": "slotSubscribe" }
            wsm.send(JSON.stringify(requestData));
        });
        wsm.addEventListener('message', (event) => {
            console.log(`Received message from server ${endpoint.nickname}: ${event.data}`);
            const data = JSON.parse(event.data);
            if (data.method === "slotNotification") {
                wsm.processSlotNotification(data.params.result.slot);
            }
        });
        wsm.addEventListener('close', () => {
            console.log(`Disconnected from server ${endpoint.nickname}`);
        });
        wsm.addEventListener('error', (err) => {
            console.error(`Error for ${endpoint.nickname}:`, err);
        });

        // Store the monitor for later use.
        window.wsMonitors[endpoint.nickname] = wsm;
        
      });
    } catch (error) {
      console.error('Error fetching websocket endpoints:', error);
    }
}

/*
    Function to disconnect all websockets.
*/
function disconnectAllWebSockets() {
    if (window.wsMonitors) {
        for (const nickname in window.wsMonitors) {
            const monitor = window.wsMonitors[nickname];
            if (monitor.readyState === WebSocket.OPEN || monitor.readyState === WebSocket.CONNECTING) {
                console.log(`Closing websocket for ${nickname}`);
                monitor.close();
            }
        }
        // Clear the monitors object.
        window.wsMonitors = {};
    }
}

/*
    Function to check whether the user has update mode paused or active and then control websockets accordingly
*/
function checkUserMonitorStatusAndControlWebSockets() {
    const toggleEl = document.getElementById("refreshToggle");
    if (toggleEl) {
        const text = toggleEl.textContent.trim();
        // console.log("Toggle text is:", text);
        if (text === "Resume Updates") {
            // If the toggle says "Resume Updates", disconnect websockets.
            disconnectAllWebSockets();
        } else if (text === "Pause Updates") {
            // If it says "Pause Updates", check if any websocket is connected.
            let connected = false;
            for (const nickname in window.wsMonitors) {
                const monitor = window.wsMonitors[nickname];
                if (monitor.readyState === WebSocket.OPEN) {
                    connected = true;
                    break;
                }
            }
            // If none are connected, reinitialize the websockets.
            if (!connected) {
                console.log("Reconnecting websockets...");
                initializeWebSockets();
            }
        }
    } else {
        console.log("Toggle element not found.");
    }
}

/* 
    Main function to initalize websocket monitoring
*/
function initializeMonitoring() {
    window.wsMonitors = window.wsMonitors || {};
    // Trigger the function every 2000 milliseconds (2 second)
    setInterval(checkUserMonitorStatusAndControlWebSockets, 2000);
}

initializeMonitoring();