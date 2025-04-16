/**
 * Extends the built-in WebSocket class to monitor response times,
 * slot notifications, and delays.
 */
class WebSocketMonitor extends WebSocket {
    /** Maximum number of entries to store in each history array. */
    static memoryLength = 150;
    /** Maximum number of entries to store in each history array. */
    static debugMode = false;

    /**
     * Creates a new WebSocketMonitor.
     *
     * @param {string} url - The WebSocket URL.
     * @param {string|string[]} protocols - Optional protocols for the WebSocket.
     * @param {string} name - A name identifier for this monitor.
     */
    constructor(url, protocols, name) {
        super(url, protocols);
        this.name = name;
        this.slot_times = [];     // Stores objects of the form { slot, notificationTime }.
        this.delay_times = [];    // Stores objects of the form { slot, delay }.
    }

    /**
     * Processes a new slot notification.
     * Calculates the time difference since the last notification,
     * updates slot times.
     *
     * @param {number} slot - The slot number from the notification.
     */
    processSlotNotification(slot) {
        const cur_time = Date.now();
        this.updateSlotTimes(slot, cur_time);
    }

    /**
     * Returns the current (latest) slot reported by this monitor.
     *
     * @returns {number} The latest slot, or -1 if none exists.
     */
    getCurrentSlot() {
        if(this.slot_times.length > 0) {
            return this.slot_times[this.slot_times.length - 1].slot;
        }
        return -1;
    }

    /**
     * Returns the earliest slot reported by this monitor.
     *
     * @returns {number} The earliest slot, or -1 if none exists.
     */
    getEarliestSlot() {
        if(this.slot_times.length > 0) {
            return this.slot_times[0].slot;
        }
        return -1;
    }

    /**
     * Updates the stored slot times with a new slot and its notification time.
     * If the stored array exceeds memoryLength, the oldest entry is removed.
     *
     * @param {number} newSlot - The new slot number.
     * @param {number} newNotificationTime - The time (in ms) of the notification.
     */
    updateSlotTimes(newSlot, newNotificationTime) {
        // If we exceed the limit, remove the oldest entry.
        if (this.slot_times.length >= WebSocketMonitor.memoryLength) {
            this.slot_times.shift();
        }
        // Store the new slot-time pair
        this.slot_times.push({ slot: newSlot, notificationTime: newNotificationTime });
    }

    /**
     * Returns the monitor's name.
     *
     * @returns {string} The name.
     */
    getName() {
        return this.name;
    }

    /**
     * Returns the notification time for the given slot from the stored slot_times.
     *
     * @param {number} slot - The slot number to search for.
     * @returns {number} The notification time, or -1 if not found.
     */
    getSlotNotificationTime(slot) {
        const entry = this.slot_times.find(item => item.slot === slot);
        return entry ? entry.notificationTime : -1;
    }

    /**
     * Calculates and returns the average delay time from the stored delay_times.
     * Filters out any invalid delay values (e.g., -1).
     *
     * @returns {number} The average delay time, or -1 if no valid delays exist.
     */
    getAverageDelayTime() {
        // Filter out invalid delays (e.g. -1) if necessary
        const validDelays = this.delay_times
          .map(entry => entry.delay)
          .filter(delay => delay >= 0);
          
        if (validDelays.length === 0) return -1;
        
        const sum = validDelays.reduce((acc, delay) => acc + delay, 0);
        return sum / validDelays.length;
      }

   /**
     * Updates the delay times for a given slot.
     * 
     * - If the slot exists in slot_times (i.e. getSlotNotificationTime(slot) returns a valid time),
     *   the delay is computed as (slotTime - earliestTime).
     * - If the slot does not exist, the delay is computed as (current time - earliestTime).
     * 
     * Then, if an entry for this slot already exists in delay_times, update its delay value.
     * If not, and if the delay_times array has reached its memory limit, remove the oldest entry,
     * then add the new delay entry.
     *
     * @param {number} slot - The slot number.
     * @param {number} earliestTime - The earliest notification time among all monitors for this slot.
     */
    updateDelayTime(slot, earliestTime) {
        // Retrieve the slot's notification time from slot_times.
        const slotTime = this.getSlotNotificationTime(slot);
        // Compute the delay:
        // If slotTime exists, use it; otherwise, use the current time.
        const computedDelay = slotTime !== -1 ? (slotTime - earliestTime) : (Date.now() - earliestTime);

        // Look for an existing entry in delay_times for this slot.
        const index = this.delay_times.findIndex(entry => entry.slot === slot);
        if (index !== -1) {
            // If an entry exists, update its delay value.
            this.delay_times[index].delay = computedDelay;
        } else {
            // Otherwise, if no entry exists, check if we need to remove the oldest entry.
            if (this.delay_times.length >= WebSocketMonitor.memoryLength) {
                this.delay_times.shift();
            }
            // Then, add a new slot-delay entry.
            this.delay_times.push({ slot: slot, delay: computedDelay });
        }
    }

    /**
     * Returns the delay time for the given slot from the stored slot_times.
     *
     * @param {number} slot - The slot number to search for.
     * @returns {number} The delay time, or -1 if not found.
     */
    getSlotDelayTime(slot) {
        const entry = this.delay_times.find(item => item.slot === slot);
        return entry ? entry.delay : -1;
    }
};

/**
 * Controller class for managing multiple WebSocketMonitor instances.
 * Responsible for initialization, disconnection, and evaluating slot notifications.
 */
class WebSocketMonitorController {

    static evaluationLength = 125;

    constructor() {
      this.wsMonitors = [];
      this.winners = [];
    }

    /**
     * Adds a WebSocketMonitor instance to the controller.
     *
     * @param {WebSocketMonitor} monitor - The monitor to add.
     */
    addMonitor(monitor) {
      this.wsMonitors.push(monitor);
    }

    /**
     * Returns the current array of monitors.
     *
     * @returns {WebSocketMonitor[]} The array of WebSocketMonitor instances.
     */
    getMonitors() {
        return this.wsMonitors;
    }


    /**
     * Disconnects all currently active WebSocketMonitor connections.
     * Closes each open or connecting WebSocket and clears the monitors array.
     */
    disconnectAllWebSockets() {
        Object.values(this.wsMonitors).forEach(monitor => {
            if (monitor.readyState === WebSocket.OPEN || monitor.readyState === WebSocket.CONNECTING) {
                console.log(`Closing websocket for ${monitor.getName()}`);
                monitor.close();
            }
        });
        // Clear the monitors object.
        this.wsMonitors = [];
    }

    /**
     * Initializes and connects all WebSocketMonitor instances by fetching endpoints.
     * Sets up event listeners for each connection.
     */
    async initializeWebSockets() {
        console.log("Reconnecting websockets...");
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
                    if(wsm.debugMode) {
                       console.log(`Received message from server ${endpoint.nickname}: ${event.data}`); 
                    }
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
                this.addMonitor(wsm);
                
            });
        } catch (error) {
            console.error('Error fetching websocket endpoints:', error);
        }
    }

    /**
     * Initializes and connects all WebSocketMonitor instances by fetching endpoints.
     * Sets up event listeners for each connection.
     */
    async getWebSocketMetrics() {
        const url = '/api/ws_metrics';
        try {
            const response = await fetch(url);
            const data = await response.json();
            console.log('Fetched websocket metrics: ', data);
            return data;
        } catch (error) {
            console.error('Error fetching websocket metrics:', error);
        }
    }

    /**
     * Checks if there is at least one active WebSocket connection.
     *
     * @returns {boolean} True if any monitor's readyState is OPEN; false otherwise.
     */
    hasActiveConnections() {
        let connected = false;
        for (const monitor of Object.values(this.wsMonitors)) {
            if (monitor.readyState === WebSocket.OPEN) {
                connected = true;
                break;
            }
        }
        return connected;
    }

    /**
     * Returns the most recent (largest) slot number reported by any monitor.
     *
     * @returns {number|null} The latest slot number, or null if no valid slots are found.
     */
    getLatestSlot() {
        if (this.wsMonitors.length === 0) {
            console.log("No monitors available");
            return null;
        }
    
        // Retrieve the current slot for each monitor.
        const currentSlots = this.wsMonitors.map(monitor => monitor.getCurrentSlot());
        
        // Filter out monitors that haven't reported a slot (-1)
        const validSlots = currentSlots.filter(slot => slot !== -1);
        
        if (validSlots.length === 0) {
            console.log("No valid slots reported yet.");
            return null;
        }
        
        // Return the maximum slot value, which represents the most recent slot.
        const latestSlot = Math.max(...validSlots);
        //console.log("Latest slot overall:", latestSlot);
        return latestSlot;
    }

    /**
     * Determines and returns the latest common slot among all monitors.
     * The common slot is defined as the minimum current slot reported by the monitors.
     *
     * @returns {number|null} The latest common slot or null if no monitors exist.
     */
    getLatestCommonSlot() {
        
        if (this.wsMonitors.length === 0) {
          console.log("No monitors available");
          return null;
        }
        
        // Map each monitor to its current slot. 
        // getCurrentSlot() returns -1 if the monitor hasn't received any slot notifications.
        const currentSlots = this.wsMonitors.map(monitor => monitor.getCurrentSlot());
        console.log(currentSlots);
        
        // If any monitor hasn't received a slot yet, you might decide to return -1 (or handle it otherwise)
        if (currentSlots.some(slot => slot === -1)) {
          console.log("At least one monitor hasn't reported a slot yet.");
          //return -1;
        }
        
        // The latest common slot is the minimum of these current slot numbers.
        const commonSlot = Math.min(...currentSlots);
        console.log("Latest common slot:", commonSlot);
        return commonSlot;
    }

    /**
     * Determines and returns the latest common slot among all monitors.
     * The common slot is defined as the minimum current slot reported by the monitors.
     *
     * @returns {number|null} The latest common slot or null if no monitors exist.
     */
    getEarliestCommonSlot() {
        
        if (this.wsMonitors.length === 0) {
          console.log("No monitors available");
          return null;
        }
        
        // Map each monitor to its current slot. 
        // getEarliestSlot() returns -1 if the monitor hasn't received any slot notifications.
        const earliestSlots = this.wsMonitors.map(monitor => monitor.getEarliestSlot());
        
        // If any monitor hasn't received a slot yet, you might decide to return -1 (or handle it otherwise)
        if (earliestSlots.some(slot => slot === -1)) {
          console.log("At least one monitor hasn't reported a slot yet.");
          //return -1;
        }
        
        // The latest common slot is the minimum of these current slot numbers.
        const commonSlot = Math.max(...earliestSlots);
        console.log("Earliest common slot:", commonSlot);
        return commonSlot;
    }

    /**
     * Helper method that returns the winner (monitor) for a given slot number.
     * The winner is the monitor that has the earliest notification time for that slot.
     * Also, after determining the winner, each monitor updates its delay times for the slot.
     *
     * @param {number} slotNumber - The slot number to evaluate.
     * @returns {WebSocketMonitor|null} The monitor with the earliest notification time, or null if none.
     */
    getWinnerForSlot(slotNumber) {
        let winnerMonitor = null;
        let earliestTime = Infinity;
        Object.values(this.wsMonitors).forEach(monitor => {
            // Look for a slot notification entry matching the given slot.
            const entry = monitor.slot_times.find(item => item.slot === slotNumber);
            if (entry) {
                if (entry.notificationTime < earliestTime) {
                    earliestTime = entry.notificationTime;
                    winnerMonitor = monitor;
                }
            }
        });
        Object.values(this.wsMonitors).forEach(monitor => {
            monitor.updateDelayTime(slotNumber, earliestTime);
        });
        return winnerMonitor;
    }

    /**
     * Calculates win rates for each monitor based on slot notifications.
     * It evaluates each slot from (latestCommonSlot - evaluationLength) to latestCommonSlot,
     * determines the monitor with the earliest notification time (the winner),
     * and then computes the win rate (percentage of slots won) for each monitor.
     *
     * @returns {Array<{name: string, winRate: number}>} Array with each monitor's name and win rate percentage.
     */
    getWinRates() {
        let latestSlot = this.getLatestSlot();
        if (latestSlot === null) {
            return null;
        }
        let earliestSlot = (latestSlot - WebSocketMonitorController.evaluationLength) + 1;

        let totalEvaluated = 0;
        let winCounts = {}; // Object to count wins per monitor (by name).

        // For each slot in the range, determine the winner.
        for (let slot = earliestSlot; slot <= latestSlot; slot++) {
            let winner = this.getWinnerForSlot(slot);
            if (winner) {
                totalEvaluated++;
                let name = winner.getName();
                winCounts[name] = (winCounts[name] || 0) + 1;
            }
        }

        // Now, calculate win rate for each monitor.
        let winRates = [];
        Object.values(this.wsMonitors).forEach(monitor => {
            let name = monitor.getName();
            let wins = winCounts[name] || 0;
            let percentage = (wins / totalEvaluated) * 100;
            winRates.push({ name, winRate: percentage, avgDelay: monitor.getAverageDelayTime() });
        });

        return winRates;
    }
  
}


/**
 * Checks whether the user has paused or active update mode and controls the websockets accordingly.
 *
 * @param {WebSocketMonitorController} controller - The controller managing the WebSocket monitors.
 */
function checkUpdateStatus(controller) {

    const toggleEl = document.getElementById("refreshToggle");
    if (toggleEl) {
        const text = toggleEl.textContent.trim();
        // console.log("Toggle text is:", text);
        if (text === "Resume Updates") {
            // If the toggle says "Resume Updates", disconnect websockets.
            controller.disconnectAllWebSockets();
        } else if (text === "Pause Updates") {
            // If it says "Pause Updates", check if any websocket is connected.
            let connected = controller.hasActiveConnections();
            // If none are connected, reinitialize the websockets.
            if (!connected) {
                console.log("No active connections, reinitializing websockets...");
                controller.initializeWebSockets();
            } else {
                console.log("Websockets already running.");
            }
        }
    } else {
        console.log("Toggle element not found.");
    }
}

/**
 * Updates the monitoring page with win rates and individual monitor status.
 *
 * It builds an ordered list of win rates (sorted by win rate, highest first)
 * and updates the container with id "webSocketWinRate".
 * It also updates each monitor's status display.
 *
 * @param {WebSocketMonitorController} controller - The controller managing the monitors.
 */
function updateMonitoringPage(controller) {

    let winRates = controller.getWinRates();
    if(winRates) {
        // Sort descending by winRate (highest first)
        winRates.sort((a, b) => b.winRate - a.winRate);

        // Build an ordered list as an HTML string.
        let html = "<ol>";
        winRates.forEach((item, index) => {
            html += `<li class="flex items-center justify-between mb-2   p-2 rounded"><span class="flex-grow">${index + 1}. ${item.name} | Win Rate: ${item.winRate.toFixed(2)}% | Avg Time Behind Winner: ${item.avgDelay.toFixed(2)} ms</span></li>`;
        });
        html += "</ol>";

        // Find the container with id "webSocketWinRate" and set its innerHTML.
        const container = document.getElementById("webSocketWinRate");
        if (container) {
            container.innerHTML = html;
        } else {
            console.log("Element with id 'webSocketWinRate' not found");
        }
    }
    

    // Update individual monitor status
    Object.values(controller.getMonitors()).forEach(monitor => {
        // Search for existing page element
        const id = 'webSocket' + monitor.getName();
        let wsListEl = document.getElementById(id);

        // If the element doesn't exist, create it and append it to a container.
        if (!wsListEl) {
            wsListEl = document.createElement('div');
            wsListEl.id = id;
            // Find the parent container
            const container = document.getElementById('webSocketList') || document.body;
            container.appendChild(wsListEl);
        }

        let date = new Date(monitor.getSlotNotificationTime(monitor.getCurrentSlot()));
        wsListEl.innerHTML = `
            <p><strong>Name:</strong> ${monitor.getName()} </p>
            <p><strong>Current Slot:</strong> ${monitor.getCurrentSlot()} </p>
            <p><strong>Slot Timestamp:</strong> ${date.toISOString()}</p>
            <p><strong>Slot Delay:</strong> ${monitor.getSlotDelayTime(monitor.getCurrentSlot())} ms</p>
            <p>--------------------------</p>
        `;
    });
}

/**
 * Updates the monitoring page with win rates and individual monitor status.
 *
 * @param {WebSocketMonitorController} controller - The controller managing the monitors.
 */
async function updateMonitoringPage2(controller) {
    const ws_metrics = await controller.getWebSocketMetrics();
    const container = document.getElementById("webSocketWinRate2");

    if (!ws_metrics) {
        console.warn("No WebSocket metrics returned from controller (null or undefined).");
        console.log(ws_metrics);
        if (container) container.innerHTML = "<p class='text-red-600'>Metrics unavailable: data not loaded yet.</p>";
        return;
    }

    if (!ws_metrics.stats) {
        console.warn("WebSocket metrics object is missing the 'stats' field.");
        if (container) container.innerHTML = "<p class='text-red-600'>Metrics format error: missing 'stats'.</p>";
        return;
    }

    if (ws_metrics.stats.length === 0) {
        console.warn("WebSocket metrics contain an empty 'stats' list.");
        if (container) container.innerHTML = "<p class='text-yellow-600'>No WebSocket activity detected in the current slot range.</p>";
        return;
    }
    // Sort descending by win_rate (defaulting to 0 if null)
    const sortedStats = ws_metrics.stats.slice().sort((a, b) => {
        const rateA = a.win_rate ?? 0;
        const rateB = b.win_rate ?? 0;
        return rateB - rateA;
    });

    // Build HTML list
    let html = "<ol>";
    sortedStats.forEach((item, index) => {
        const nickname = item.nickname;
        const winRate = (item.win_rate ?? 0).toFixed(2);
        const avgDelay = item.avg_delay !== null && item.avg_delay !== undefined
            ? item.avg_delay.toFixed(2)
            : "N/A";
        html += `
            <li class="flex items-center justify-between mb-2 p-2 rounded">
                <span class="flex-grow">
                    ${index + 1}. ${nickname} | Win Rate: ${winRate}% | Avg Delay: ${avgDelay} ms
                </span>
            </li>
        `;
    });
    html += "</ol>";

    // Insert into DOM
    if (container) {
        container.innerHTML = html;
    } else {
        console.warn("Element with id 'webSocketWinRate' not found");
    }

    // Update individual monitor status from ws_metrics.stats
    const listContainer = document.getElementById('webSocketList2') || document.body;

    // Clear old content (optional, if you want to fully rebuild each time)
    listContainer.innerHTML = '';

    ws_metrics.stats.forEach((stat) => {
        const id = `webSocket2${stat.nickname}`;
        let wsListEl = document.getElementById(id);

        if (!wsListEl) {
            wsListEl = document.createElement('div');
            wsListEl.id = id;
            wsListEl.className = 'p-3 border rounded mb-2';
            listContainer.appendChild(wsListEl);
        }
›
        const slot = stat.last_slot_update?.slot_info?.slot ?? 'N/A';
        const timestamp = stat.last_slot_update?.timestamp
            ? new Date(stat.last_slot_update.timestamp * 1000).toISOString()
            : 'N/A';
        const delay = stat.last_slot_update?.delay ?? 'N/A';

        wsListEl.innerHTML = `
            <p><strong>Name:</strong> ${stat.nickname}</p>
            <p><strong>Current Slot:</strong> ${slot}</p>
            <p><strong>Slot Timestamp:</strong> ${timestamp}</p>
            <p><strong>Slot Delay:</strong> ${delay.toFixed ? delay.toFixed(2) + ' ms' : delay}</p>
            <p class="text-gray-400">--------------------------</p>
        `;
    });
}

/**
 * Main initialization function for websocket monitoring.
 * It creates the WebSocketMonitorController and starts periodic updates.
 */
function initializeMonitoring() {
    window.wsmController = new WebSocketMonitorController();
    // Trigger the function every 2000 milliseconds (2 second)
    setInterval(() => checkUpdateStatus(window.wsmController), 2000);
    setInterval(() => updateMonitoringPage(window.wsmController), 500);
    setInterval(() => updateMonitoringPage2(window.wsmController), 250);
}

initializeMonitoring();