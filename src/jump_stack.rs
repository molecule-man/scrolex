#[derive(Debug)]
pub(crate) struct JumpStack {
    buffer: [u32; 100], // The ring buffer of fixed size
    head: usize,        // Points to the next insertion point
    start: usize,       // Points to the oldest item
    count: usize,       // Tracks the number of items in the stack
}

impl JumpStack {
    pub(crate) fn new() -> Self {
        JumpStack {
            buffer: [0; 100], // Initialize all elements to 0
            head: 0,          // Start at the beginning of the buffer
            start: 0,         // Start at the beginning of the buffer
            count: 0,         // No elements in the stack
        }
    }

    pub(crate) fn push(&mut self, value: u32) {
        self.buffer[self.head] = value; // Write the value to the current head

        // Move the head forward circularly
        self.head = (self.head + 1) % 100;

        if self.count == 100 {
            // If the buffer is full, move the start pointer forward to overwrite the oldest item
            self.start = (self.start + 1) % 100;
        } else {
            self.count += 1;
        }
    }

    pub(crate) fn pop(&mut self) -> Option<u32> {
        if self.is_empty() {
            return None;
        }

        // Move head backward circularly
        self.head = (self.head + 99) % 100;

        // Get the value from the buffer
        let value = self.buffer[self.head];

        // Decrease the count, since an item was removed
        self.count -= 1;

        Some(value)
    }

    /// Peek at the top value of the stack without removing it
    pub(crate) fn peek(&self) -> Option<u32> {
        if self.is_empty() {
            return None;
        }

        // Get the index of the most recent item, which is one before head
        let top_index = (self.head + 99) % 100;
        Some(self.buffer[top_index])
    }

    /// Reset the stack, clearing all elements by adjusting the pointers
    pub(crate) fn reset(&mut self) {
        self.head = 0;
        self.start = 0;
        self.count = 0;
    }

    fn is_empty(&self) -> bool {
        self.count == 0
    }
}

impl Default for JumpStack {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_push_single_item() {
        let mut stack = JumpStack::new();
        stack.push(42);
        assert_eq!(stack.peek(), Some(42), "Peek should return the pushed item");
        assert!(!stack.is_empty(), "Stack should not be empty after push");
    }

    #[test]
    fn test_push_and_pop_single_item() {
        let mut stack = JumpStack::new();
        stack.push(42);
        assert_eq!(stack.pop(), Some(42), "Pop should return the pushed item");
        assert_eq!(stack.pop(), None, "Stack should be empty after pop");
        assert_eq!(stack.peek(), None, "Stack should be empty after pop");
    }

    #[test]
    fn test_push_multiple_items() {
        let mut stack = JumpStack::new();
        stack.push(1);
        stack.push(2);
        stack.push(3);
        assert_eq!(
            stack.peek(),
            Some(3),
            "Peek should return the most recently pushed item"
        );
        assert_eq!(
            stack.pop(),
            Some(3),
            "Pop should return the most recently pushed item"
        );
        assert_eq!(
            stack.peek(),
            Some(2),
            "Peek should now return the next item"
        );
    }

    #[test]
    fn test_pop_from_empty_stack() {
        let mut stack = JumpStack::new();
        assert_eq!(
            stack.pop(),
            None,
            "Pop should return None from an empty stack"
        );
        stack.push(42);
        stack.pop();
        assert_eq!(
            stack.pop(),
            None,
            "Pop should return None from an empty stack after all elements are popped"
        );
    }

    #[test]
    fn test_reset_stack() {
        let mut stack = JumpStack::new();
        stack.push(1);
        stack.push(2);
        stack.push(3);
        stack.reset();
        assert_eq!(stack.peek(), None, "Peek should return None after reset");
        assert_eq!(stack.pop(), None, "Pop should return None after reset");
    }

    #[test]
    fn test_push_overwrite_oldest() {
        let mut stack = JumpStack::new();

        // Fill the stack with 100 items
        for i in 1..=100 {
            stack.push(i);
        }

        // Push one more item to overwrite the oldest (1)
        stack.push(101);

        // After 101 pushes, the oldest item (1) should be replaced by 101
        assert_eq!(
            stack.peek(),
            Some(101),
            "Peek should return the most recent item (101)"
        );

        // Check that items before are correct
        assert_eq!(stack.pop(), Some(101), "Pop should return 101");
        assert_eq!(stack.pop(), Some(100), "Pop should return 100");
    }

    #[test]
    fn test_push_and_wraparound_behavior() {
        let mut stack = JumpStack::new();

        // Push 100 items
        for i in 1..=100 {
            stack.push(i);
        }

        // Now push another 100 items to ensure wraparound occurs
        for i in 101..=200 {
            stack.push(i);
        }

        // The stack should hold values 101 to 200
        assert_eq!(
            stack.pop(),
            Some(200),
            "The top of the stack should be 200 after wraparound"
        );
        assert_eq!(
            stack.pop(),
            Some(199),
            "The next item should be 199 after wraparound"
        );
        assert_eq!(
            stack.pop(),
            Some(198),
            "The next item should be 198 after wraparound"
        );

        // Check the bottom-most item (which should be 101, since it overwrote the old values)
        for _ in 4..100 {
            stack.pop();
        }
        assert_eq!(
            stack.pop(),
            Some(101),
            "The bottom-most item should be 101 after wraparound"
        );
    }
}
