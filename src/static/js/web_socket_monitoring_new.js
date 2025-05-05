/**
 * Controller class responsible for retrieving WebSocket metrics from the Rust server
 * and updating the frontend user interface accordingly.
 */
class WebSocketMetricsController {

    /**
     * Initializes the controller with a default active refresh state.
     */
    constructor() {
        this.refresh_active = true;
    }

    /**
     * Sets the status of the periodic UI refresh.
     *
     * @param {boolean} status - `true` to enable refreshing, `false` to pause updates.
     */
    setRefreshStatus(status) {
        this.refresh_active = status;
    }

    /**
     * Gets the current refresh status.
     *
     * @returns {boolean} `true` if refreshing is enabled, `false` otherwise.
     */
    getRefreshStatus() {
        return this.refresh_active;
    }

    /**
     * Fetches current WebSocket metrics from the Rust backend API.
     *
     * @returns {Promise<Object|undefined>} The parsed metrics JSON or `undefined` on error.
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
     * Updates the monitoring UI with the latest WebSocket metrics.
     * Handles win rate leaderboard and detailed endpoint statistics.
     */
    async updateMonitoringPage() {

        if (!this.getRefreshStatus()) {
            console.log("⏸️ Skipping updateMonitoringPage (paused)");
            return;
        }

        const ws_metrics = await this.getWebSocketMetrics();
        const container = document.getElementById("webSocketWinRate2");

        if (!ws_metrics) {
            console.warn("No WebSocket metrics returned from controller (null or undefined).");
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


        this.updateWinRatesLeaderboard(ws_metrics);
        this.updateSlotDetails(ws_metrics);

    }

    /**
     * Renders the sorted WebSocket win rate leaderboard into the DOM.
     *
     * @param {Object} ws_metrics - The metrics object returned from the backend.
     */
    updateWinRatesLeaderboard(ws_metrics) {

        const fromSlotElement = document.getElementById('fromSlot');
        if (fromSlotElement) {
            fromSlotElement.textContent = `${ws_metrics.start_slot}`;
        }
        const toSlotElement = document.getElementById('toSlot');
        if (toSlotElement) {
            toSlotElement.textContent = `${ws_metrics.latest_slot}`;
        }

        const sortedStats = ws_metrics.stats.slice().sort((a, b) => {
            const rateA = a.win_rate ?? 0;
            const rateB = b.win_rate ?? 0;
            return rateB - rateA;
        });
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
                        ${index + 1}. ${nickname} | Win Rate: ${winRate}% | Avg Time Behind Winner: ${avgDelay} ms
                    </span>
                </li>
            `;
        });
        html += "</ol>";
    
        const container = document.getElementById("webSocketWinRate2");
        if (container) {
            container.innerHTML = html;
        } else {
            console.warn("Element with id 'webSocketWinRate2' not found");
        }
    }

    /**
     * Displays detailed information for each WebSocket endpoint, including slot,
     * timestamp, and delay.
     *
     * @param {Object} ws_metrics - The metrics object returned from the backend.
     */
    updateSlotDetails(ws_metrics) {
        const listContainer = document.getElementById('webSocketList2') || document.body;
        listContainer.innerHTML = '';
    
        // Sort alphabetically by nickname
        const sortedStats = ws_metrics.stats.slice().sort((a, b) => {
            return a.nickname.localeCompare(b.nickname);
        });
    
        sortedStats.forEach((stat) => {
            const id = `webSocket2${stat.nickname}`;
            let wsListEl = document.getElementById(id);
    
            if (!wsListEl) {
                wsListEl = document.createElement('div');
                wsListEl.id = id;
                wsListEl.className = 'p-3 border rounded mb-2';
                listContainer.appendChild(wsListEl);
            }
    
            const slot = stat.slot_details?.slot_info?.slot ?? 'N/A';
            const timestamp = (stat.slot_details?.timestamp > 0)
                ? new Date(stat.slot_details.timestamp * 1000).toISOString()
                : 'N/A';
            const delay = stat.slot_details?.delay ?? 'N/A';
    
            wsListEl.innerHTML = `
                <p><strong>Name:</strong> ${stat.nickname}</p>
                <p><strong>Slot:</strong> ${slot}</p>
                <p><strong>Slot Timestamp:</strong> ${timestamp}</p>
                <p><strong>Slot Delay:</strong> ${delay.toFixed ? delay.toFixed(2) + ' ms' : delay}</p>
            `;
        });
    }
}


/**
 * Checks the current UI toggle state to determine whether updates should
 * be paused or resumed. Updates the controller accordingly.
 *
 * @param {WebSocketMetricsController} controller - The WebSocket controller instance.
 */
function checkUpdateStatus(controller) {

    const toggleEl = document.getElementById("refreshToggle");
    if (toggleEl) {
        const text = toggleEl.textContent.trim();
        // console.log("Toggle text is:", text);
        if (text === "Resume Updates") {
            // If the toggle says "Resume Updates", disconnect websockets.
            controller.setRefreshStatus(false);
        } else if (text === "Pause Updates") {
            controller.setRefreshStatus(true);
        }
    } else {
        console.log("Toggle element not found.");
    }
}

/**
 * Initializes the monitoring controller and starts periodic update loops.
 */
function initializeMonitoring() {
    window.wsMetricsController = new WebSocketMetricsController();

    setInterval(() => checkUpdateStatus(window.wsMetricsController), 2000);
    setInterval(() => window.wsMetricsController.updateMonitoringPage(), 1000);
}

initializeMonitoring();